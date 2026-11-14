use std::borrow::Cow;

use crate::{diagnostic::LanguageServerId, snippets::RenderedSnippet, Transaction};

#[derive(Debug, PartialEq, Clone)]
pub struct CompletionItem {
    pub transaction: Transaction,
    pub label: Cow<'static, str>,
    pub kind: Cow<'static, str>,
    /// Containing Markdown
    pub documentation: Option<String>,
    pub provider: CompletionProvider,
    /// Snippet tabstops for the inserted text, when the provider expands a
    /// snippet instead of plain text.
    pub snippet: Option<RenderedSnippet>,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum CompletionProvider {
    Lsp(LanguageServerId),
    Path,
    Word,
    Snippet,
}

impl From<LanguageServerId> for CompletionProvider {
    fn from(id: LanguageServerId) -> Self {
        CompletionProvider::Lsp(id)
    }
}
