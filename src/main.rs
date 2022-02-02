use lazy_static::lazy_static;
use mail_parser::*;
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use truncrate::TruncateToBoundary;
use twilight_embed_builder::{EmbedAuthorBuilder, EmbedBuilder};
use twilight_http::Client as HttpClient;
use twilight_model::channel::{
    embed::{Embed, EmbedField},
    thread::AutoArchiveDuration,
};
use twilight_model::datetime::Timestamp;

type ChannelId = twilight_model::id::Id<twilight_model::id::marker::ChannelMarker>;

lazy_static! {
    static ref DB: sled::Db = sled::Config::new().path(env::var("DB_PATH").unwrap()).use_compression(true).open().unwrap();
    static ref MAIL_STORAGE: sled::Tree = DB.open_tree("storage").unwrap();
    static ref THREADS: sled::Tree = DB.open_tree("threads").unwrap();
    static ref INBOX_CHANNEL_ID: ChannelId = env::var("INBOX_CHANNEL").ok().and_then(|v| v.parse::<u64>().ok()).and_then(ChannelId::new_checked).unwrap();
    static ref DOMAIN_COLORS: HashMap<String, u32> = {
        if let Ok(domain_string) = env::var("DOMAIN_COLORS") {
            domain_string
                .split_terminator(',')
                .filter_map(|domain| {
                    domain.split_once(':').and_then(|(d, color)| {
                        u32::from_str_radix(color, 16)
                            .ok()
                            .map(|c| (d.to_owned(), c))
                    })
                })
                .collect::<HashMap<String, u32>>()
        } else {
            HashMap::new()
        }
    };

    // lol. lmao
    static ref GMAIL_QUOTE_REGEX: Regex = Regex::new(r"On (Mon|Tue|Wed|Thu|Fri|Sat|Sun), (Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d+, \d{4} at \d{1,2}:\d{1,2} (AM|PM) .*<.+> wrote:").unwrap();
}

macro_rules! addr {
    ($e:expr) => {
        if let HeaderValue::Address(a) = $e {
            Some(a)
        } else {
            None
        }
    };
}

macro_rules! render_addr_list {
    ($e:expr) => {
        match $e {
            HeaderValue::Address(a) => a.address.as_ref().map(|v| format!("{}", v)),
            HeaderValue::AddressList(l) => {
                let v = l
                    .into_iter()
                    .filter_map(|a| a.address.as_ref().map(|v| v.to_string()))
                    .collect::<Vec<String>>()
                    .join(",");
                if !v.is_empty() {
                    Some(v)
                } else {
                    None
                }
            }
            _ => None,
        }
    };
}

fn get_addr_color(header: &HeaderValue) -> u32 {
    addr!(header)
        .and_then(|a| a.address.as_ref())
        .and_then(|v| v.split_once('@'))
        .and_then(|(_, rhs)| DOMAIN_COLORS.get(rhs).copied())
        .unwrap_or(0x5275b3)
}

fn build_embed(message: &Message) -> Result<Vec<Embed>, Box<dyn Error + Send + Sync>> {
    let mut message_contents: String = message
        .text_body
        .iter()
        .filter_map(|part_id| message.get_text_body(*part_id))
        .map(|s| {
            s.lines()
                .filter(|v| !v.trim_start().starts_with('>') && !GMAIL_QUOTE_REGEX.is_match(v))
                .collect::<Vec<&str>>()
                .join("\n")
        })
        .collect::<String>();

    let color = get_addr_color(message.get_from());

    let mut message_parts: Vec<String> = Vec::new();

    while !message_contents.is_empty() {
        let (_, idx) = message_contents.slice_indices_at_offset(4096);
        message_parts.push(message_contents.drain(..idx).collect());
    }

    let mut starting_embed = EmbedBuilder::new()
    .author(
        EmbedAuthorBuilder::new(format!(
            "{} to {}",
            addr!(message.get_from())
                .and_then(|v| v.address.as_ref())
                .map(|v| v.to_string())
                .unwrap_or(String::from("unknown address")),
            render_addr_list!(message.get_to()).unwrap_or(String::from("unknown address"))
        ))
        .build(),
    )
    .color(color)
    .timestamp(message.get_date().and_then(|v| Timestamp::parse(&v.to_iso8601()).ok()).unwrap_or_else(|| {
        Timestamp::from_secs(SystemTime::now().duration_since(UNIX_EPOCH).expect("what did you do to your system clock? i cannot get duration from the unix epoch").as_secs() as i64).expect("failed to parse the current date into a discord timestamp")
    }))
    .title(message.get_subject().unwrap())
    .description(message_parts.remove(0))
    .build()?;

    if let Some(cc) = render_addr_list!(message.get_cc()) {
        starting_embed.fields.push(EmbedField {
            inline: true,
            name: "cc:".to_owned(),
            value: cc,
        });
    }

    let mut embeds: Vec<Embed> = vec![starting_embed];

    for text in message_parts {
        embeds.push(EmbedBuilder::new().color(color).description(text).build()?);
    }

    Ok(embeds)
}

async fn send_email(
    http: Arc<HttpClient>,
    email_bytes: impl AsRef<[u8]>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    // parse email
    let email = mail_parser::Message::parse(email_bytes.as_ref()).unwrap();
    let message_id = email.get_message_id().unwrap().as_bytes();

    // put email into storage in case we need it someday
    MAIL_STORAGE.insert(message_id, email_bytes.as_ref())?;

    let from = addr!(email.get_from()).unwrap();

    // message is a followup
    if let Some(previous) = email.get_in_reply_to().as_text_ref() {
        // if we have started a discord thread for this line of emails already, use that
        if let Some(thread_id) = THREADS.get(previous.as_bytes())? {
            let channel_id =
                ChannelId::new(u64::from_be_bytes(thread_id.as_ref().try_into().unwrap()));
            let embeds = build_embed(&email)?;
            for embed in embeds {
                http.create_message(channel_id)
                    .embeds(&[embed])?
                    .exec()
                    .await?
                    .model()
                    .await?;
            }

            THREADS.insert(message_id, thread_id)?;

            return Ok(());
        }
    }

    // message is the start of a new thread, so let's create that
    let start_msg = http
        .create_message(*INBOX_CHANNEL_ID)
        .content(&format!(
            "**new thread - started by {}**\n{}",
            from.address.as_ref().unwrap(),
            email.get_subject().unwrap()
        ))?
        .exec()
        .await?
        .model()
        .await?;

    // actually create the thread in the inbox channel
    let thread = http
        .create_thread_from_message(
            *INBOX_CHANNEL_ID,
            start_msg.id,
            email.get_subject().unwrap(),
        )?
        .auto_archive_duration(AutoArchiveDuration::Day)
        .exec()
        .await?
        .model()
        .await?;

    // send embeds
    let embeds = build_embed(&email)?;
    for embed in embeds {
        http.create_message(thread.id())
            .embeds(&[embed])?
            .exec()
            .await?;
    }

    THREADS.insert(message_id, &thread.id().get().to_be_bytes())?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let token = env::var("DISCORD_TOKEN")?;
    let http = Arc::new(HttpClient::new(token));

    let server = tiny_http::Server::http("0.0.0.0:8010").unwrap();
    for mut request in server.incoming_requests() {
        let mut bytes = Vec::with_capacity(request.body_length().unwrap_or(100));
        request.as_reader().read_to_end(&mut bytes).unwrap();

        if !bytes.is_empty() {
            tokio::task::spawn(send_email(Arc::clone(&http), bytes));
        }

        request.respond(tiny_http::Response::empty(204i16)).unwrap();
    }

    Ok(())
}
