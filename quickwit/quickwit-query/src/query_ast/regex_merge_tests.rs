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

use tantivy::schema::{STRING, Schema};

use super::*;
use crate::query_ast::TermQuery;

fn regex(field: &str, pattern: &str) -> QueryAst {
    RegexQuery::from_field_value(field, pattern).into()
}

fn schema() -> Schema {
    let mut builder = Schema::builder();
    builder.add_text_field("url", STRING);
    builder.add_json_field("json", STRING);
    builder.build()
}

#[test]
fn test_regex_merge_groups_by_full_field_name() {
    let schema = schema();
    let context = BuildTantivyAstContext::for_test(&schema);
    let untouched = [
        TermQuery::from_field_value("url", "keep").into(),
        regex("json.b", "solo"),
    ];
    let mut should = vec![
        regex("url", "foo"),
        regex("json.a", "foo"),
        regex("url", "bar"),
        regex("json.a", "bar"),
    ];
    should.extend(untouched.clone());
    let merged = merge_regexes(
        BoolQuery {
            should,
            ..Default::default()
        }
        .into(),
        &context,
    );
    assert_eq!(merge_regexes(merged.clone(), &context), merged);
    let QueryAst::Bool(merged) = merged else {
        panic!("expected bool")
    };
    assert_eq!(merged.should.len(), 4);
    for clause in untouched {
        assert!(merged.should.contains(&clause));
    }
    let mut fields: Vec<_> = merged
        .should
        .iter()
        .filter_map(|clause| match clause {
            QueryAst::Regex(regex) => Some(regex.field.as_str()),
            _ => None,
        })
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["json.a", "json.b", "url"]);
}

#[test]
fn test_regex_merge_only_unions() {
    let schema = schema();
    let context = BuildTantivyAstContext::for_test(&schema);
    let alternatives = vec![regex("url", "foo"), regex("url", "bar")];
    for minimum in [1, 2] {
        let original = BoolQuery {
            must: alternatives.clone(),
            filter: alternatives.clone(),
            should: alternatives.clone(),
            must_not: alternatives.clone(),
            minimum_should_match: Some(minimum),
        };
        let QueryAst::Bool(merged) = merge_regexes(original.into(), &context) else {
            panic!("expected bool")
        };
        assert_eq!(merged.must, alternatives);
        assert_eq!(merged.filter, alternatives);
        assert_eq!(merged.minimum_should_match, Some(minimum));
        assert_eq!(merged.must_not.len(), 1);
        if minimum == 2 {
            assert_eq!(merged.should, alternatives);
        } else {
            assert_eq!(merged.should.len(), 1);
        }
    }
}

#[test]
fn test_regex_merge_keeps_unparseable_group() {
    let schema = schema();
    let context = BuildTantivyAstContext::for_test(&schema);
    let original: QueryAst = BoolQuery {
        should: vec![regex("url", ".*"), regex("url", "(")],
        ..Default::default()
    }
    .into();
    let merged = merge_regexes(original.clone(), &context);
    assert_eq!(merged, original);
    assert!(merged.build_tantivy_query(&context).is_err());
}
