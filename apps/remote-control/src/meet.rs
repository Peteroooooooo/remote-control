//! Meeting: the small talk between the cores of two computers on their own
//! ALPN, before and besides any desktop session. Each side keeps a
//! connection to each device it shows a card for and says hello on it
//! (`Msg::MeetHi`); the answer says whether the other side knows it
//! (`MeetWelcome`) or not (`MeetRefuse`). A hello also redeems a connection
//! code (its token) and carries changes to the device lists, as does a
//! welcome: "let us remember each other" (asked, then agreed or refused on
//! the other side), "I added you" and "I deleted you" (the other side
//! deletes this computer too).

use std::time::Duration;

use anyhow::{Result, bail};
use iroh::endpoint::{Connection, RecvStream, SendStream};

use crate::proto::{Msg, read_msg};

pub const ALPN: &[u8] = b"rc/meet/1";

pub const EVENT_NONE: u8 = 0;
/// I put you in my device list.
pub const EVENT_REMEMBER: u8 = 1;
/// I deleted you from my device list.
pub const EVENT_FORGET: u8 = 2;
/// I want us to remember each other (you are not in my list yet).
pub const EVENT_ASK: u8 = 3;
/// I did not agree to remember each other.
pub const EVENT_NO: u8 = 4;

/// I have not met you (no code, not on my LAN, not in my list).
pub const REFUSE_UNKNOWN: u8 = 1;
/// I deleted you from my device list.
pub const REFUSE_FORGOTTEN: u8 = 2;
/// The connection code is over an hour old and was never used.
pub const REFUSE_CODE_EXPIRED: u8 = 3;
/// Another device used the connection code first.
pub const REFUSE_CODE_USED: u8 = 4;
/// The code was revoked, or its sender restarted since.
pub const REFUSE_CODE_UNKNOWN: u8 = 5;

pub fn refusal_text(reason: u8) -> &'static str {
    match reason {
        REFUSE_FORGOTTEN => "对方已把这台电脑从设备列表里删除",
        REFUSE_CODE_EXPIRED => "连接码已过期（超过 1 小时没人用），请对方重新复制一个",
        REFUSE_CODE_USED => "连接码已被别的设备用过了，请对方重新复制一个",
        REFUSE_CODE_UNKNOWN => "连接码已被撤销，或者对方重启过软件，请对方重新复制一个",
        _ => "对方不认识这台电脑",
    }
}

/// Sends one hello on its own stream and waits for the answer.
pub async fn ask(conn: &Connection, hi: &Msg) -> Result<Msg> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(&hi.encode()).await?;
    let _ = send.finish();
    let m = tokio::time::timeout(Duration::from_secs(5), read_msg(&mut recv)).await;
    match m {
        Ok(Ok(m @ (Msg::MeetWelcome { .. } | Msg::MeetRefuse { .. }))) => Ok(m),
        Ok(Ok(m)) => bail!("unexpected answer {m:?}"),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("no answer within 5 s"),
    }
}

/// The next hello on an incoming meet connection.
pub async fn next_hi(conn: &Connection) -> Result<Option<(Msg, SendStream)>> {
    let (send, mut recv): (SendStream, RecvStream) = match conn.accept_bi().await {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };
    let m = tokio::time::timeout(Duration::from_secs(5), read_msg(&mut recv)).await;
    match m {
        Ok(Ok(m @ Msg::MeetHi { .. })) => Ok(Some((m, send))),
        Ok(Ok(m)) => bail!("expected a hello, got {m:?}"),
        Ok(Err(e)) => Err(e),
        Err(_) => bail!("no hello within 5 s"),
    }
}

pub async fn answer(mut send: SendStream, m: &Msg) -> Result<()> {
    send.write_all(&m.encode()).await?;
    let _ = send.finish();
    Ok(())
}
