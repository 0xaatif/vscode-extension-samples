use std::{
    borrow::Cow,
    cell::OnceCell,
    collections::{hash_map::Entry, HashMap},
    iter,
    sync::{Mutex, MutexGuard},
};

use lazy_regex::regex;
use line_index::{LineIndex, TextSize, WideEncoding, WideLineCol};
use serde_json::{json, Value};
use tower_lsp::{
    jsonrpc,
    lsp_types::{
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        InitializeParams, InitializeResult, PartialResultParams, Position, Range, SemanticToken,
        SemanticTokenType, SemanticTokens, SemanticTokensFullOptions, SemanticTokensLegend,
        SemanticTokensOptions, SemanticTokensParams, SemanticTokensResult,
        SemanticTokensServerCapabilities, ServerCapabilities, TextDocumentContentChangeEvent,
        TextDocumentIdentifier, TextDocumentItem, TextDocumentSyncCapability, TextDocumentSyncKind,
        VersionedTextDocumentIdentifier, WorkDoneProgressOptions, WorkDoneProgressParams,
    },
    LspService, Server,
};
use tracing::{debug, warn};
use url::Url;

fn main() -> anyhow::Result<()> {
    let (svc, socket) = LspService::new(|_client| {
        LanguageServer(Mutex::new(State {
            documents: HashMap::new(),
        }))
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(Server::new(tokio::io::stdin(), tokio::io::stdout(), socket).serve(svc));
    Ok(())
}

struct State {
    /// Stores the latest version of a document.
    documents: HashMap<Url, (i32, SourceFile)>,
}

struct LanguageServer(Mutex<State>);

impl LanguageServer {
    fn state(&self) -> MutexGuard<State> {
        self.0.lock().unwrap()
    }
}

#[tower_lsp::async_trait]
impl tower_lsp::LanguageServer for LanguageServer {
    // lifecycle
    // ---------

    async fn initialize(
        &self,
        _params: InitializeParams,
    ) -> Result<InitializeResult, jsonrpc::Error> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            work_done_progress_options: WorkDoneProgressOptions {
                                work_done_progress: None,
                            },
                            legend: SemanticTokensLegend {
                                // MUST match the order in `enum TokenKind`, below
                                token_types: vec![SemanticTokenType::KEYWORD],
                                token_modifiers: vec![],
                            },
                            range: None,
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                        },
                    ),
                ),
                ..Default::default()
            },
            server_info: None,
        })
    }
    async fn shutdown(&self) -> Result<(), jsonrpc::Error> {
        Ok(())
    }

    // text update
    // -----------

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let DidOpenTextDocumentParams {
            text_document:
                TextDocumentItem {
                    uri,
                    language_id: _,
                    version,
                    text,
                },
        } = params;
        self.state()
            .documents
            .insert(uri, (version, SourceFile::new(text)));
    }
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes,
        } = params;
        for TextDocumentContentChangeEvent {
            range,
            range_length,
            text,
        } in content_changes
        {
            match (range, range_length) {
                (None, None) => {
                    match self.state().documents.entry(uri.clone()) {
                        Entry::Occupied(mut already) => {
                            let (existing_version, _) = already.get();
                            match version > *existing_version {
                                true => {
                                    already.insert((version, SourceFile::new(text)));
                                }
                                false => {
                                    debug!(%uri, %version, %existing_version, "ignoring state didChange")
                                }
                            }
                        }
                        Entry::Vacant(space) => {
                            warn!(%uri, %version, "no such document on didChange");
                            space.insert((version, SourceFile::new(text)));
                        }
                    };
                }
                _ => warn!(%uri, %version, "ignoring relative didChange"),
            }
        }
    }
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        } = params;
        if self.state().documents.remove(&uri).is_none() {
            warn!(%uri, "no such document on didClose")
        }
    }

    // features
    // --------

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>, jsonrpc::Error> {
        let SemanticTokensParams {
            work_done_progress_params: WorkDoneProgressParams { work_done_token },
            partial_result_params:
                PartialResultParams {
                    partial_result_token,
                },
            text_document: TextDocumentIdentifier { uri },
        } = params;
        ensure!(work_done_token.is_none());
        ensure!(partial_result_token.is_none());
        let Some((_, text)) = self.state().documents.get(&uri).cloned() else {
            bail!("no such document with uri {}", uri)
        };
        ensure!(text.source.len() <= u32::MAX as usize);
        // `\b` is a word boundary
        let builder = regex!(r#"\b(zero|knowledge|proof)\b"#)
            .find_iter(&text.source)
            .filter_map(|matsh| {
                let range = text.range(matsh.start(), matsh.end())?;
                Some((range, TokenKind::Keyword))
            })
            .collect::<SemanticTokensBuilder>();
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: builder.data,
        })))
    }
}

/// <https://github.com/rust-lang/rust-analyzer/blob/c88ea11832277b6c010088d658965c39c1181d20/crates/rust-analyzer/src/lsp/semantic_tokens.rs#L183-L230>
#[derive(Default)]
struct SemanticTokensBuilder {
    prev_line: u32,
    prev_char: u32,
    data: Vec<SemanticToken>,
}

impl Extend<(Range, TokenKind)> for SemanticTokensBuilder {
    fn extend<II: IntoIterator<Item = (Range, TokenKind)>>(&mut self, iter: II) {
        for (range, token_kind) in iter {
            let mut push_line = range.start.line;
            let mut push_char = range.start.character;

            if !self.data.is_empty() {
                push_line -= self.prev_line;
                if push_line == 0 {
                    push_char -= self.prev_char;
                }
            }

            // A token cannot be multiline
            let token_len = range.end.character - range.start.character;

            let token = SemanticToken {
                delta_line: push_line,
                delta_start: push_char,
                length: token_len,
                token_type: token_kind as u32,
                token_modifiers_bitset: 0,
            };

            self.data.push(token);

            self.prev_line = range.start.line;
            self.prev_char = range.start.character;
        }
    }
}

impl FromIterator<(Range, TokenKind)> for SemanticTokensBuilder {
    fn from_iter<II: IntoIterator<Item = (Range, TokenKind)>>(iter: II) -> Self {
        let mut this = Self::default();
        this.extend(iter);
        this
    }
}

#[derive(Clone)]
struct SourceFile {
    source: String,
    /// LSP requires utf-16 offsets.
    ///
    /// Defer the work of indexing until it's actually needed.
    offset_lookup: OnceCell<LineIndex>,
}

impl SourceFile {
    fn new(source: String) -> Self {
        Self {
            source,
            offset_lookup: OnceCell::new(),
        }
    }
    fn range(&self, start: usize, end: usize) -> Option<Range> {
        let lookup = self.offset_lookup();
        let offset2position = |offset: usize| {
            let WideLineCol { line, col } = lookup.to_wide(
                WideEncoding::Utf16,
                lookup.line_col(TextSize::try_from(offset).ok()?),
            )?;
            Some(Position {
                line,
                character: col,
            })
        };
        Some(Range {
            start: offset2position(start)?,
            end: offset2position(end)?,
        })
    }
    fn offset_lookup(&self) -> &LineIndex {
        self.offset_lookup
            .get_or_init(|| LineIndex::new(&self.source))
    }
}

/// Ordering MUST match the [`SemanticTokensLegend`] provided in [`tower_lsp::LanguageServer::initialize`].
#[repr(u32)]
enum TokenKind {
    Keyword,
}

/// Accept [`anyhow::Error`] (`: !Error`) _and_ other error types.
fn conv_error(e: impl Into<Box<dyn std::error::Error>>) -> jsonrpc::Error {
    let e = e.into();
    jsonrpc::Error {
        code: (-1).into(),
        message: Cow::Owned(e.to_string()),
        data: e.source().map(|_| {
            json!({
                "chain": Value::Array(
                    iter::successors(Some(&*e as &dyn std::error::Error), |it| it.source())
                        .map(|e| Value::String(e.to_string())).collect())
            })
        }),
    }
}

macro_rules! ensure {
	($($tt:tt)*) => {
		::core::result::Result::map_err(
			(|| -> ::anyhow::Result<()> {::anyhow::ensure!($($tt)*); ::core::result::Result::Ok(())})(),
			$crate::conv_error
		)?;
	};
}
pub(crate) use ensure;

macro_rules! bail {
	($($tt:tt)*) => {
		return ::core::result::Result::Err($crate::conv_error(::anyhow::anyhow!($($tt)*)))
	};
}
pub(crate) use bail;
