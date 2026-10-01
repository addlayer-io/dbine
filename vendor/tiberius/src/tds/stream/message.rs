//! PATCH(dbine): a batch's whole response in order, as SSMS shows it.
//!
//! [`QueryStream`](crate::QueryStream) yields only result metadata and rows,
//! and ends with the first server error. A [`MessageStream`] also yields the
//! server's messages (INFO: `PRINT`, `RAISERROR` up to severity 10,
//! `SET STATISTICS IO/TIME`, warnings), every ERROR token, each statement's
//! DONE (with its row count unless `SET NOCOUNT ON`) and the database and
//! transaction changes (ENVCHANGE), so a client can print
//! "(3 rows affected)", every `Msg …` and follow `USE`.

use crate::tds::codec::{TokenDone, TokenEnvChange, TokenError, TokenInfo};
use crate::tds::stream::ReceivedToken;
use crate::{row::ColumnType, Column, ResultMetadata, Row};
use futures_util::{
    ready,
    stream::{BoxStream, Stream, StreamExt},
};
use std::{
    fmt::Debug,
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};

/// A server message: an INFO or ERROR token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerMessage {
    /// Message number (`Msg N`); 0 for `PRINT`.
    pub number: u32,
    /// Error state (`State`).
    pub state: u8,
    /// Severity (`Level`): INFO up to 10, ERROR from 11.
    pub class: u8,
    /// The text.
    pub message: String,
    /// The server that sent it.
    pub server: String,
    /// The module it came from, empty for the batch itself.
    pub procedure: String,
    /// 1-based line in the batch, or in `procedure` when there is one.
    pub line: u32,
}

impl From<TokenInfo> for ServerMessage {
    fn from(t: TokenInfo) -> Self {
        Self {
            number: t.number,
            state: t.state,
            class: t.class,
            message: t.message,
            server: t.server,
            procedure: t.procedure,
            line: t.line,
        }
    }
}

impl From<TokenError> for ServerMessage {
    fn from(t: TokenError) -> Self {
        Self {
            number: t.code,
            state: t.state,
            class: t.class,
            message: t.message,
            server: t.server,
            procedure: t.procedure,
            line: t.line,
        }
    }
}

/// Which DONE token ended a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoneKind {
    /// A statement of the batch (or the batch's end).
    Done,
    /// A stored procedure's end.
    DoneProc,
    /// A statement inside a stored procedure or trigger.
    DoneInProc,
}

/// A statement ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DoneInfo {
    /// Which DONE token it was.
    pub kind: DoneKind,
    /// Rows it returned or changed; `None` when the server sent no count
    /// (`SET NOCOUNT ON`, statements without one).
    pub rows: Option<u64>,
    /// More results follow.
    pub more: bool,
    /// It failed.
    pub error: bool,
    /// The statement's token (`CurCmd`).
    pub command: u16,
}

impl DoneInfo {
    fn new(kind: DoneKind, d: &TokenDone) -> Self {
        Self {
            kind,
            rows: d.count(),
            more: d.is_more(),
            error: d.is_error(),
            command: d.cur_cmd(),
        }
    }
}

/// One item of a [`MessageStream`].
#[derive(Debug)]
pub enum MessageItem {
    /// A result set starts.
    Metadata(ResultMetadata),
    /// A row of the current result set.
    Row(Row),
    /// An INFO token.
    Info(ServerMessage),
    /// An ERROR token. The response goes on after it.
    Error(ServerMessage),
    /// A DONE, DONEPROC or DONEINPROC token.
    Done(DoneInfo),
    /// The session's database changed (`USE`): the new name.
    Database(String),
    /// A transaction began (`true`) or ended (`false`).
    Transaction(bool),
}

/// The response to [`Client::simple_query_messages`], read to the end (or
/// dropped) before the connection is used again. It ends with
/// [`Error::Cancelled`](crate::error::Error::Cancelled) when a
/// [`CancelHandle`](crate::CancelHandle) stopped the batch.
///
/// [`Client::simple_query_messages`]: crate::Client::simple_query_messages
pub struct MessageStream<'a> {
    token_stream: BoxStream<'a, crate::Result<ReceivedToken>>,
    columns: Option<Arc<Vec<Column>>>,
    result_set_index: Option<usize>,
}

impl<'a> Debug for MessageStream<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageStream").finish()
    }
}

impl<'a> MessageStream<'a> {
    pub(crate) fn new(token_stream: BoxStream<'a, crate::Result<ReceivedToken>>) -> Self {
        Self {
            token_stream,
            columns: None,
            result_set_index: None,
        }
    }
}

impl<'a> Stream for MessageStream<'a> {
    type Item = crate::Result<MessageItem>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            let token = match ready!(this.token_stream.poll_next_unpin(cx)) {
                Some(Ok(t)) => t,
                Some(Err(e)) => return Poll::Ready(Some(Err(e))),
                None => return Poll::Ready(None),
            };
            let item = match token {
                ReceivedToken::NewResultset(meta) => {
                    let columns: Vec<Column> = meta
                        .columns
                        .iter()
                        .map(|x| Column {
                            name: x.col_name.to_string(),
                            column_type: ColumnType::from(&x.base.ty),
                        })
                        .collect();
                    let columns = Arc::new(columns);
                    this.columns = Some(columns.clone());
                    this.result_set_index = Some(this.result_set_index.map_or(0, |i| i + 1));
                    MessageItem::Metadata(ResultMetadata {
                        columns,
                        result_index: this.result_set_index.unwrap_or(0),
                    })
                }
                ReceivedToken::Row(data) => {
                    let Some(columns) = this.columns.clone() else {
                        return Poll::Ready(Some(Err(crate::Error::Protocol(
                            "ROW token arrived before any column metadata".into(),
                        ))));
                    };
                    MessageItem::Row(Row {
                        columns,
                        data,
                        result_index: this.result_set_index.unwrap_or(0),
                    })
                }
                ReceivedToken::Done(d) | ReceivedToken::DoneProc(d) | ReceivedToken::DoneInProc(d)
                    if d.is_attention() =>
                {
                    return Poll::Ready(Some(Err(crate::Error::Cancelled)));
                }
                ReceivedToken::Done(d) => MessageItem::Done(DoneInfo::new(DoneKind::Done, &d)),
                ReceivedToken::DoneProc(d) => MessageItem::Done(DoneInfo::new(DoneKind::DoneProc, &d)),
                ReceivedToken::DoneInProc(d) => MessageItem::Done(DoneInfo::new(DoneKind::DoneInProc, &d)),
                ReceivedToken::Info(i) => MessageItem::Info(i.into()),
                ReceivedToken::Error(e) => MessageItem::Error(e.into()),
                ReceivedToken::EnvChange(TokenEnvChange::Database { new, .. }) => MessageItem::Database(new),
                ReceivedToken::EnvChange(TokenEnvChange::BeginTransaction(_)) => MessageItem::Transaction(true),
                ReceivedToken::EnvChange(
                    TokenEnvChange::CommitTransaction
                    | TokenEnvChange::RollbackTransaction
                    | TokenEnvChange::DefectTransaction,
                ) => MessageItem::Transaction(false),
                _ => continue,
            };
            return Poll::Ready(Some(Ok(item)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tds::codec::{BaseMetaDataColumn, FixedLenType, MetaDataColumn, TokenColMetaData, TokenRow, TypeInfo};
    use futures_util::stream::{self, TryStreamExt};
    use std::borrow::Cow;

    fn info(number: u32, class: u8, message: &str) -> TokenInfo {
        TokenInfo {
            number,
            state: 1,
            class,
            message: message.into(),
            server: "srv".into(),
            procedure: String::new(),
            line: 2,
        }
    }

    fn meta() -> ReceivedToken {
        let col = MetaDataColumn {
            base: BaseMetaDataColumn {
                flags: enumflags2::BitFlags::empty(),
                ty: TypeInfo::FixedLen(FixedLenType::Int4),
                table_name: None,
            },
            col_name: Cow::Borrowed("c"),
        };
        ReceivedToken::NewResultset(Arc::new(TokenColMetaData { columns: vec![col] }))
    }

    async fn items(tokens: Vec<ReceivedToken>) -> crate::Result<Vec<MessageItem>> {
        MessageStream::new(stream::iter(tokens.into_iter().map(Ok::<_, crate::Error>)).boxed()).try_collect().await
    }

    #[tokio::test]
    async fn messages_counts_and_errors_come_in_order() {
        let got = items(vec![
            ReceivedToken::Info(info(0, 0, "hola")),
            meta(),
            ReceivedToken::Row(TokenRow::new()),
            // DONE_MORE | DONE_COUNT, SELECT, 1 row.
            ReceivedToken::Done(TokenDone::from_parts(0x11, 0xC1, 1)),
            ReceivedToken::Error(TokenError {
                code: 2627,
                state: 1,
                class: 14,
                message: "dup".into(),
                server: "srv".into(),
                procedure: String::new(),
                line: 3,
            }),
            ReceivedToken::Info(info(3621, 0, "The statement has been terminated.")),
            // DONE_MORE | DONE_ERROR, INSERT, no count.
            ReceivedToken::Done(TokenDone::from_parts(0x03, 0xC3, 0)),
            ReceivedToken::EnvChange(TokenEnvChange::Database { old: "a".into(), new: "b".into() }),
            ReceivedToken::EnvChange(TokenEnvChange::BeginTransaction([1; 8])),
            ReceivedToken::EnvChange(TokenEnvChange::RollbackTransaction),
            ReceivedToken::ReturnStatus(0),
            ReceivedToken::Done(TokenDone::from_parts(0, 0, 0)),
        ])
        .await
        .unwrap();
        let shape: Vec<String> = got
            .iter()
            .map(|i| match i {
                MessageItem::Metadata(m) => format!("meta {}", m.result_index()),
                MessageItem::Row(_) => "row".into(),
                MessageItem::Info(m) => format!("info {} {}", m.number, m.message),
                MessageItem::Error(m) => format!("error {} {} line {}", m.number, m.class, m.line),
                MessageItem::Done(d) => format!("done {:?} {} {}", d.rows, d.more, d.error),
                MessageItem::Database(d) => format!("db {d}"),
                MessageItem::Transaction(t) => format!("tx {t}"),
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                "info 0 hola",
                "meta 0",
                "row",
                "done Some(1) true false",
                "error 2627 14 line 3",
                "info 3621 The statement has been terminated.",
                "done None true true",
                "db b",
                "tx true",
                "tx false",
                "done None false false",
            ]
        );
    }

    #[tokio::test]
    async fn the_attention_ack_ends_it_cancelled() {
        // DONE_ATTN.
        let r = items(vec![ReceivedToken::Done(TokenDone::from_parts(0x20, 0, 0))]).await;
        assert!(matches!(r, Err(crate::Error::Cancelled)));
    }
}
