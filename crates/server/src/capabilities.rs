//! What Javelin tells the editor it can do.
//!
//! Capabilities are advertised for what the server *will* answer, not for what
//! is warm yet: a feature does not disappear from the handshake and reappear
//! later, because most clients only read capabilities once. The vocabulary for
//! saying "incomplete" out loud exists — [`javelin_protocol::Answer`] — but
//! nothing in `javelin-server` constructs one yet, because the handlers that
//! exist answer from the open buffer or not at all.

use lsp_types::{
    CallHierarchyServerCapability, CompletionOptions, FoldingRangeProviderCapability,
    HoverProviderCapability, ImplementationProviderCapability, OneOf, PositionEncodingKind,
    SaveOptions, SelectionRangeProviderCapability, SemanticTokensFullOptions,
    SemanticTokensOptions, SemanticTokensServerCapabilities, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, TypeDefinitionProviderCapability,
};

use crate::handlers;

/// The position encoding Javelin implements.
///
/// UTF-16 is the LSP default and what `javelin-document`'s line index computes.
/// Advertising anything else would require changing that first.
pub const POSITION_ENCODING: PositionEncodingKind = PositionEncodingKind::UTF16;

pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(POSITION_ENCODING),
        // Incremental sync is the whole point: re-sending a 5000-line file on
        // every keystroke would defeat incremental parsing downstream. Saves
        // are asked for — without the text, which the buffer already has —
        // because a save is when the workspace index re-reads a file, and a
        // client sends `didSave` only to a server that said it wants one.
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::INCREMENTAL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(false),
                })),
                ..TextDocumentSyncOptions::default()
            },
        )),
        // Syntax-level and answerable from the moment a file parses, which is
        // why it is the first feature Javelin advertises.
        document_symbol_provider: Some(OneOf::Left(true)),
        // Both answer for names the open file declares and null for everything
        // else. Advertising them is honest at that scope; the answers get better
        // as later phases teach the engine about the rest of the workspace.
        definition_provider: Some(OneOf::Left(true)),
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        // `.` is advertised because member resolution answers it: without it a
        // client only asks after an identifier character, so completion after a
        // dot is never requested and the feature is unreachable from a real
        // editor. A receiver this file cannot type answers an empty list, which
        // clients render as no popup at all.
        completion_provider: Some(CompletionOptions {
            trigger_characters: Some(vec![".".to_string()]),
            ..CompletionOptions::default()
        }),
        // Answered from the workspace source index, which arrives on a
        // background thread after the handshake. Advertised anyway, by the
        // rule at the top of this file: the index lands seconds in, and a
        // symbol picker that was told "no" at startup never asks again.
        // Before it lands the answer is an empty list.
        workspace_symbol_provider: Some(OneOf::Left(true)),
        // Folding ranges are computed from the syntax tree alone and are
        // available immediately after a file is parsed.
        folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
        // Selection ranges allow expanding/contracting selection through
        // syntactic ancestors, computed from the syntax tree.
        selection_range_provider: Some(SelectionRangeProviderCapability::Simple(true)),
        // Every occurrence of the binding under the cursor, computed from the
        // scope tree alone — no classpath and no workspace index needed.
        document_highlight_provider: Some(OneOf::Left(true)),
        // Types across every indexed file, everything else within the open
        // one. Advertised before the workspace walk lands, by the rule at the
        // top of this file; until then a type's references are the buffer's.
        references_provider: Some(OneOf::Left(true)),
        // `full` and `range`, no `delta`. The legend comes
        // from `handlers::semantic_tokens::LEGEND` so the indices the handler
        // emits cannot drift from what is advertised here.
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: handlers::semantic_tokens::legend(),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                range: Some(true),
                ..SemanticTokensOptions::default()
            },
        )),
        // The inverse edges, read out of the workspace index: an interface's
        // implementations, a method's overrides, and — below — the type and
        // call hierarchies. Advertised before the walk lands, by the rule at
        // the top of this file; until then they answer nothing.
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        // The declaration of the type a value has, placed as definition
        // places a type name.
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        call_hierarchy_provider: Some(CallHierarchyServerCapability::Simple(true)),
        // A parameter's name before each argument, for the calls in the
        // range asked about.
        inlay_hint_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}

/// [`server_capabilities`] as the `initialize` response sends it, with the
/// one capability `lsp-types` has no field for: `typeHierarchyProvider`,
/// added in LSP 3.17 and absent from the crate's `ServerCapabilities`.
pub fn capabilities_json() -> javelin_core::Result<serde_json::Value> {
    let mut capabilities = serde_json::to_value(server_capabilities()).map_err(|err| {
        javelin_core::Error::internal(format!("capabilities are not serialisable: {err}"))
    })?;
    capabilities["typeHierarchyProvider"] = serde_json::Value::Bool(true);
    Ok(capabilities)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `lsp-types` has no field for it, so it is added to the JSON; a client
    /// that is not told never offers the type hierarchy.
    #[test]
    fn the_type_hierarchy_is_advertised_in_the_json_sent() {
        let json = capabilities_json().expect("serialisable");
        assert_eq!(json["typeHierarchyProvider"], serde_json::json!(true));
        assert_eq!(json["implementationProvider"], serde_json::json!(true));
        assert_eq!(json["callHierarchyProvider"], serde_json::json!(true));
    }

    #[test]
    fn sync_is_incremental_and_asks_for_saves_without_their_text() {
        let Some(TextDocumentSyncCapability::Options(options)) =
            server_capabilities().text_document_sync
        else {
            panic!("the options form, which is the only one that can ask for saves");
        };
        assert_eq!(options.change, Some(TextDocumentSyncKind::INCREMENTAL));
        assert_eq!(options.open_close, Some(true));
        assert!(matches!(
            options.save,
            Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                include_text: Some(false)
            }))
        ));
    }

    #[test]
    fn the_outline_is_advertised() {
        assert!(matches!(
            server_capabilities().document_symbol_provider,
            Some(OneOf::Left(true))
        ));
    }

    #[test]
    fn local_navigation_is_advertised() {
        let capabilities = server_capabilities();
        assert!(matches!(
            capabilities.definition_provider,
            Some(OneOf::Left(true))
        ));
        assert!(matches!(
            capabilities.hover_provider,
            Some(HoverProviderCapability::Simple(true))
        ));
    }

    #[test]
    fn a_dot_is_advertised_so_member_completion_is_actually_requested() {
        let completion = server_capabilities()
            .completion_provider
            .expect("completion is implemented");
        assert_eq!(
            completion.trigger_characters,
            Some(vec![".".to_string()]),
            "member resolution is unreachable from a client that is not told to ask after a dot"
        );
    }

    #[test]
    fn folding_ranges_are_advertised() {
        assert!(
            matches!(
                server_capabilities().folding_range_provider,
                Some(FoldingRangeProviderCapability::Simple(true))
            ),
            "folding ranges are available from the syntax tree"
        );
    }

    #[test]
    fn selection_ranges_are_advertised() {
        assert!(
            matches!(
                server_capabilities().selection_range_provider,
                Some(SelectionRangeProviderCapability::Simple(true))
            ),
            "selection ranges are available from the syntax tree"
        );
    }

    #[test]
    fn document_highlight_is_advertised() {
        assert!(
            matches!(
                server_capabilities().document_highlight_provider,
                Some(OneOf::Left(true))
            ),
            "document highlight is available from the scope tree alone"
        );
    }

    #[test]
    fn nothing_that_is_not_implemented_is_advertised() {
        let capabilities = server_capabilities();
        assert!(capabilities.rename_provider.is_none());
    }

    #[test]
    fn semantic_tokens_full_and_range_are_advertised_and_delta_is_not() {
        match server_capabilities().semantic_tokens_provider {
            Some(SemanticTokensServerCapabilities::SemanticTokensOptions(options)) => {
                assert_eq!(
                    options.full,
                    Some(SemanticTokensFullOptions::Bool(true)),
                    "full without delta"
                );
                assert_eq!(options.range, Some(true));
            }
            other => panic!("semantic tokens capability missing or an unexpected shape: {other:?}"),
        }
    }

    #[test]
    fn references_are_advertised() {
        assert!(
            matches!(
                server_capabilities().references_provider,
                Some(OneOf::Left(true))
            ),
            "a client that is not told the server answers references never asks"
        );
    }

    #[test]
    fn workspace_symbols_are_advertised() {
        assert!(
            matches!(
                server_capabilities().workspace_symbol_provider,
                Some(OneOf::Left(true))
            ),
            "a symbol picker that is not told the server has one never asks"
        );
    }

    #[test]
    fn the_advertised_encoding_matches_what_the_line_index_computes() {
        assert_eq!(
            server_capabilities().position_encoding,
            Some(PositionEncodingKind::UTF16)
        );
    }
}
