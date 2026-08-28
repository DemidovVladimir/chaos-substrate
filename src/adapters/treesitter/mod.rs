//! STUB — tree-sitter SourceExtractor adapter. To be implemented.

#[cfg(test)]
mod grammar_smoke {
    use tree_sitter::{Language, Parser};

    fn parse(lang: Language, src: &str) -> String {
        let mut p = Parser::new();
        p.set_language(&lang).expect("grammar ABI compatible");
        p.parse(src, None).unwrap().root_node().to_sexp()
    }

    #[test]
    fn all_grammars_load() {
        assert!(parse(tree_sitter_rust::LANGUAGE.into(), "fn a() {}").contains("function_item"));
        assert!(parse(tree_sitter_typescript::LANGUAGE_TSX.into(), "const a = () => 1;").contains("arrow_function"));
        assert!(parse(tree_sitter_javascript::LANGUAGE.into(), "function a() {}").contains("function_declaration"));
        assert!(parse(tree_sitter_python::LANGUAGE.into(), "def a(): pass").contains("function_definition"));
        assert!(parse(tree_sitter_solidity::LANGUAGE.into(), "contract A {}").contains("contract_declaration"));
        assert!(parse(tree_sitter_graphql::LANGUAGE.into(), "type Query { a: Int }").contains("object_type_definition"));
    }
}
