// Copyright 2021-Present Datadog, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::convert::Infallible;
use std::ops::Range;

use anyhow::{Context, bail};
use regex_syntax::ast::{Ast, Visitor, visit};

use crate::tokenizers::TokenizerManager;

pub(super) fn wildcard_to_regex(mut wildcard: &str, case_insensitive: bool) -> String {
    // Explicit flags prevent RegexQuery from adding the field's automatic case folding.
    let mut regex = if case_insensitive { "(?i)" } else { "(?-i)" }.to_string();
    let append_literal = |regex: &mut String, text: &str| {
        // Keep the original literal boundaries, including escaped characters. Normalizers may
        // reject long tokens, so joining separately normalized literals would change behavior.
        regex.push_str("(?:");
        regex.push_str(&regex::escape(text));
        regex.push(')');
    };
    while let Some(pos) = wildcard.find(['*', '?', '\\']) {
        if pos > 0 {
            append_literal(&mut regex, &wildcard[..pos]);
        }
        let operator = wildcard.as_bytes()[pos];
        wildcard = &wildcard[pos + 1..];
        match operator {
            b'*' => regex.push_str(".*"),
            b'?' => regex.push('.'),
            b'\\' => {
                if let Some(character) = wildcard.chars().next() {
                    append_literal(&mut regex, &wildcard[..character.len_utf8()]);
                    wildcard = &wildcard[character.len_utf8()..];
                } else {
                    // As before, ignore a trailing escape.
                    break;
                }
            }
            _ => unreachable!("find only returns wildcard operators and escapes"),
        }
    }
    if !wildcard.is_empty() {
        append_literal(&mut regex, wildcard);
    }
    regex
}

struct LiteralRun {
    range: Range<usize>,
    text: String,
}

#[derive(Default)]
struct LiteralRuns(Vec<LiteralRun>);

impl Visitor for LiteralRuns {
    type Err = Infallible;
    type Output = Vec<LiteralRun>;

    fn finish(self) -> Result<Self::Output, Self::Err> {
        Ok(self.0)
    }

    fn visit_pre(&mut self, ast: &Ast) -> Result<(), Self::Err> {
        if let Ast::Literal(literal) = ast {
            let range = literal.span.start.offset..literal.span.end.offset;
            if let Some(previous) = self.0.last_mut()
                && previous.range.end == range.start
            {
                previous.range.end = range.end;
                previous.text.push(literal.c);
            } else {
                self.0.push(LiteralRun {
                    range,
                    text: literal.c.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Normalize literal runs before regex flags are interpreted. Case folding in HIR is too late:
/// for example, `(?i)S` has already become a character class rather than a literal at that point.
pub(super) fn normalize_literals(
    regex: &str,
    tokenizer_name: &str,
    tokenizer_manager: &TokenizerManager,
) -> anyhow::Result<String> {
    let mut normalizer = tokenizer_manager
        .get_normalizer(tokenizer_name)
        .with_context(|| format!("no tokenizer named `{tokenizer_name}` is registered"))?;
    let ast = regex_syntax::ast::parse::Parser::new().parse(regex)?;
    let Ok(literals) = visit(&ast, LiteralRuns::default());
    let mut normalized = String::with_capacity(regex.len());
    let mut offset = 0;
    for literal in literals {
        let mut token_stream = normalizer.token_stream(&literal.text);
        let text = token_stream
            .next()
            .context("normalizer generated no content")?
            .text
            .clone();
        if token_stream.next().is_some() {
            bail!("normalizer generated multiple tokens");
        }
        normalized.push_str(&regex[offset..literal.range.start]);
        normalized.push_str(&regex::escape(&text));
        offset = literal.range.end;
    }
    normalized.push_str(&regex[offset..]);
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use tantivy::schema::{Schema as TantivySchema, TextFieldIndexing, TextOptions};

    use crate::query_ast::{RegexQuery, ResolvedRegex};
    use crate::{InvalidQuery, create_default_quickwit_tokenizer_manager};

    fn single_text_field_schema(field_name: &str, tokenizer: &str) -> TantivySchema {
        let mut schema_builder = TantivySchema::builder();
        let text_options = TextOptions::default()
            .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
        schema_builder.add_text_field(field_name, text_options);
        schema_builder.build()
    }

    #[test]
    fn test_wildcard_query_to_regex_on_text() {
        let query = RegexQuery::from_wildcard(
            "text_field".to_string(),
            "MyString Wh1ch?a.nOrMal Tokenizer would*cut",
            false,
        );

        let tokenizer_manager = create_default_quickwit_tokenizer_manager();
        for tokenizer in ["raw", "whitespace"] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_text_field("text_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?-i)(?:MyString Wh1ch).(?:a\\.nOrMal Tokenizer would).*(?:cut)"
            );
            assert!(json_path.is_none());
        }

        for tokenizer in [
            "raw_lowercase",
            "lowercase",
            "default",
            "chinese_compatible",
            "source_code_default",
            "source_code_with_hex",
        ] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_text_field("text_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?-i)(?:mystring wh1ch).(?:a\\.normal tokenizer would).*(?:cut)"
            );
            assert!(json_path.is_none());
        }
    }

    #[test]
    fn test_wildcard_query_to_regex_on_escaped_text() {
        let query = RegexQuery::from_wildcard(
            "text_field".to_string(),
            "MyString Wh1ch\\?a.nOrMal Tokenizer would\\*cut",
            false,
        );

        let tokenizer_manager = create_default_quickwit_tokenizer_manager();
        for tokenizer in ["raw", "whitespace"] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_text_field("text_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?-i)(?:MyString Wh1ch)(?:\\?)(?:a\\.nOrMal Tokenizer would)(?:\\*)(?:cut)"
            );
            assert!(json_path.is_none());
        }

        for tokenizer in [
            "raw_lowercase",
            "lowercase",
            "default",
            "chinese_compatible",
            "source_code_default",
            "source_code_with_hex",
        ] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_text_field("text_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?-i)(?:mystring wh1ch)(?:\\?)(?:a\\.normal tokenizer would)(?:\\*)(?:cut)"
            );
            assert!(json_path.is_none());
        }
    }

    #[test]
    fn test_wildcard_query_to_regex_on_json() {
        // Keep case and regex metacharacters in the JSON path unchanged.
        let query = RegexQuery::from_wildcard(
            "json_field.Inner.Fie*ld".to_string(),
            "MyString Wh1ch?a.nOrMal Tokenizer would*cut",
            false,
        );

        let tokenizer_manager = create_default_quickwit_tokenizer_manager();
        for tokenizer in ["raw", "whitespace"] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_json_field("json_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?-i)(?:MyString Wh1ch).(?:a\\.nOrMal Tokenizer would).*(?:cut)"
            );
            assert_eq!(json_path.unwrap(), "Inner\u{1}Fie*ld\0s".as_bytes());
        }

        for tokenizer in [
            "raw_lowercase",
            "lowercase",
            "default",
            "chinese_compatible",
            "source_code_default",
            "source_code_with_hex",
        ] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_json_field("json_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?-i)(?:mystring wh1ch).(?:a\\.normal tokenizer would).*(?:cut)"
            );
            assert_eq!(json_path.unwrap(), "Inner\u{1}Fie*ld\0s".as_bytes());
        }
    }

    #[test]
    fn test_extract_regex_wildcard_missing_field() {
        let query =
            RegexQuery::from_wildcard("my_missing_field".to_string(), "My query value*", false);
        let tokenizer_manager = create_default_quickwit_tokenizer_manager();
        let schema = single_text_field_schema("my_field", "whitespace");
        let err = query
            .to_resolved(&schema, Some(&tokenizer_manager))
            .unwrap_err();
        let InvalidQuery::FieldDoesNotExist {
            full_path: missing_field_full_path,
        } = err
        else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(missing_field_full_path, "my_missing_field");
    }

    #[test]
    fn test_wildcard_query_to_regex_on_text_case_insensitive() {
        let query = RegexQuery::from_wildcard(
            "text_field".to_string(),
            "MyString Wh1ch?a.nOrMal Tokenizer would*cut",
            true,
        );

        let tokenizer_manager = create_default_quickwit_tokenizer_manager();
        for tokenizer in ["raw", "whitespace"] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_text_field("text_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?i)(?:MyString Wh1ch).(?:a\\.nOrMal Tokenizer would).*(?:cut)"
            );
            assert!(json_path.is_none());
        }

        for tokenizer in [
            "raw_lowercase",
            "lowercase",
            "default",
            "chinese_compatible",
            "source_code_default",
            "source_code_with_hex",
        ] {
            let mut schema_builder = TantivySchema::builder();
            let text_options = TextOptions::default()
                .set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer));
            schema_builder.add_text_field("text_field", text_options);
            let schema = schema_builder.build();

            let ResolvedRegex {
                json_path, regex, ..
            } = query
                .to_resolved(&schema, Some(&tokenizer_manager))
                .unwrap();
            assert_eq!(
                regex,
                "(?i)(?:mystring wh1ch).(?:a\\.normal tokenizer would).*(?:cut)"
            );
            assert!(json_path.is_none());
        }
    }
}
