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

use std::net::IpAddr;

use itertools::Itertools;
use serde_json::Value as JsonValue;
use tantivy::schema::{Field, IntoIpv6Addr, OwnedValue};

use super::{
    LeafType, MappingLeaf, MappingNode, MappingTree, NumVal, check_supplied_tie_breaker_value,
};
use crate::QuickwitValue::Leaf;
use crate::quickwit_doc::QuickwitLeafValue::{Null, U64};
use crate::quickwit_doc::{ByteDictionary, FieldBindings, QuickwitLeafValue, QuickwitObject};
use crate::{Cardinality, DocParsingError, ModeType, QuickwitValue};

// Paths store key IDs because converting a leaf can grow the dictionary during traversal.
fn field_name(path: &[u32], dictionary: &ByteDictionary) -> String {
    path.iter().map(|id| dictionary.get_str(*id)).join(".")
}

impl MappingNode {
    pub(in crate::doc_mapper) fn map_tantivy_fields(
        &self,
        object: QuickwitObject<'_>,
        dictionary: &mut ByteDictionary,
        mode: ModeType,
        path: &mut Vec<u32>,
        bindings: &mut FieldBindings,
    ) -> Result<(), DocParsingError> {
        for (key, entry_offset, value_offset, value) in object.entries() {
            path.push(key);
            let name = dictionary.get_str(key);
            match self.branches.get(name) {
                Some(MappingTree::Leaf(leaf)) => {
                    leaf.bind_values(value, value_offset, dictionary, mode, path, bindings)?;
                }
                Some(MappingTree::Node(node)) => match value {
                    QuickwitValue::Object(child) => {
                        let header =
                            (mode == ModeType::Dynamic).then(|| bindings.begin_dynamic_object(key));
                        node.map_tantivy_fields(child, dictionary, mode, path, bindings)?;
                        if let Some(header) = header {
                            bindings.end_dynamic_object(header);
                        }
                    }
                    QuickwitValue::Array(items)
                        if items.iter().all(|item| matches!(item, Leaf(Null))) => {}
                    Leaf(Null) => {}
                    _ => {
                        let input = json_input(value, dictionary)
                            .map(|json| json.to_string())
                            .unwrap_or_else(|error| error);
                        return Err(DocParsingError::ValueError(
                            field_name(path, dictionary),
                            format!("expected an JSON object, got {input}"),
                        ));
                    }
                },
                None if mode == ModeType::Strict => {
                    return Err(DocParsingError::NoSuchFieldInSchema(field_name(
                        path, dictionary,
                    )));
                }
                None if mode == ModeType::Dynamic => bindings.push_dynamic_whole(entry_offset),
                None => {}
            }
            path.pop();
        }
        Ok(())
    }

    // Dynamic contributions follow all explicitly mapped values in a concatenate field.
    pub(in crate::doc_mapper) fn concatenate_dynamic(
        &self,
        object: QuickwitObject<'_>,
        dictionary: &ByteDictionary,
        fields: &[Field],
        bindings: &mut FieldBindings,
    ) {
        for (key, _, value_offset, value) in object.entries() {
            match (self.branches.get(dictionary.get_str(key)), value) {
                (None, _) => bind_json_primitives(value, value_offset, fields, bindings),
                (Some(MappingTree::Node(node)), QuickwitValue::Object(child)) => {
                    node.concatenate_dynamic(child, dictionary, fields, bindings)
                }
                _ => (),
            }
        }
    }
}

impl MappingLeaf {
    fn bind_values(
        &self,
        value: QuickwitValue<'_>,
        value_offset: u32,
        dictionary: &mut ByteDictionary,
        mode: ModeType,
        path: &[u32],
        bindings: &mut FieldBindings,
    ) -> Result<(), DocParsingError> {
        if matches!(self.typ, LeafType::TieBreaker) {
            return check_supplied_tie_breaker_value(
                matches!(value, Leaf(Null)),
                mode == ModeType::Strict,
                field_name(path, dictionary),
            );
        }
        match value {
            QuickwitValue::Array(items) => {
                if self.cardinality == Cardinality::SingleValued {
                    return Err(DocParsingError::MultiValuesNotSupported(field_name(
                        path, dictionary,
                    )));
                }
                for (item_offset, item) in items.entries() {
                    self.bind_item(item, item_offset, dictionary, path, bindings)?;
                }
            }
            scalar => {
                self.bind_item(scalar, value_offset, dictionary, path, bindings)?;
            }
        }
        Ok(())
    }

    fn bind_item(
        &self,
        item: QuickwitValue<'_>,
        item_offset: u32,
        dictionary: &mut ByteDictionary,
        path: &[u32],
        bindings: &mut FieldBindings,
    ) -> Result<(), DocParsingError> {
        if matches!(item, Leaf(Null)) {
            return Ok(());
        }
        let target_fields = self.concatenate.iter().copied().chain([self.field]);
        let converted = self
            .typ
            .convert_quickwit_value(item, dictionary)
            .map_err(|err| DocParsingError::ValueError(field_name(path, dictionary), err))?;
        if let Some(converted) = converted {
            for field in target_fields {
                bindings.bind_computed(field, converted);
            }
            return Ok(());
        }
        if matches!(self.typ, LeafType::Json(_)) {
            if !self.concatenate.is_empty() {
                bind_json_primitives(item, item_offset, &self.concatenate, bindings);
            }
            bindings.bind(self.field, item_offset, true);
        } else {
            for field in target_fields {
                bindings.bind(field, item_offset, false);
            }
        }
        Ok(())
    }
}

impl LeafType {
    /// `None` binds the original value. Only a change of type requires a new value.
    fn convert_quickwit_value(
        &self,
        value: QuickwitValue<'_>,
        dictionary: &mut ByteDictionary,
    ) -> Result<Option<QuickwitLeafValue>, String> {
        use QuickwitLeafValue as Scalar;
        let text = |id| dictionary.get_str(id);
        let converted = match (self, value) {
            (Self::Text(_), Leaf(Scalar::String(_)))
            | (Self::Bool(_), Leaf(Scalar::Bool(_)))
            | (Self::I64(_), Leaf(Scalar::I64(_)))
            | (Self::U64(_), Leaf(Scalar::U64(_)))
            | (Self::F64(_), Leaf(Scalar::F64(_)))
            | (Self::Json(_), QuickwitValue::Object(_)) => return Ok(None),
            (Self::I64(options), Leaf(Scalar::String(id))) => {
                Scalar::I64(i64::from_numeric_string(text(id), options.coerce)?)
            }
            (Self::U64(options), Leaf(Scalar::String(id))) => {
                Scalar::U64(u64::from_numeric_string(text(id), options.coerce)?)
            }
            (Self::F64(options), Leaf(Scalar::String(id))) => {
                Scalar::F64(f64::from_numeric_string(text(id), options.coerce)?)
            }
            (Self::DateTime(options), Leaf(Scalar::String(id))) => {
                Scalar::DateTime(options.parse_str(text(id))?)
            }
            (Self::Bytes(options), Leaf(Scalar::String(id))) => {
                let bytes = options.input_format.parse_str(text(id))?;
                Scalar::Bytes(dictionary.intern(&bytes)?)
            }
            (Self::IpAddr(_), Leaf(Scalar::String(id))) => {
                let text = text(id);
                let ip = text
                    .parse::<IpAddr>()
                    .map_err(|err| format!("failed to parse IP address `{text}`: {err}"))?;
                Scalar::IpAddr(dictionary.intern(&ip.into_ipv6_addr().octets())?)
            }
            (Self::TieBreaker, _) => unreachable!("tie breakers are generated, not converted"),
            // Reuse the JSON mapper's number range checks, datetime formats and type errors.
            _ => match self.value_from_json(json_input(value, dictionary)?)? {
                OwnedValue::I64(number) => Scalar::I64(number),
                OwnedValue::U64(number) => Scalar::U64(number),
                OwnedValue::F64(number) => Scalar::F64(number),
                OwnedValue::Date(timestamp) => Scalar::DateTime(timestamp),
                _ => unreachable!("all other valid leaf values are handled above"),
            },
        };
        Ok(Some(converted))
    }
}

// Concatenate indexes primitive JSON values. Unlike JSON object fields it keeps RFC3339 strings
// as text; positive integers that fit i64 use Tantivy's signed JSON number representation.
fn bind_json_primitives(
    value: QuickwitValue<'_>,
    value_offset: u32,
    fields: &[Field],
    bindings: &mut FieldBindings,
) {
    match value {
        QuickwitValue::Array(items) => {
            for (child_offset, child) in items.entries() {
                bind_json_primitives(child, child_offset, fields, bindings);
            }
        }
        QuickwitValue::Object(object) => {
            for (_, _, child_offset, child) in object.entries() {
                bind_json_primitives(child, child_offset, fields, bindings);
            }
        }
        Leaf(Null) => {}
        Leaf(leaf) => {
            let is_json = matches!(leaf, U64(_));
            for &field in fields {
                bindings.bind(field, value_offset, is_json);
            }
        }
    }
}

fn json_input(value: QuickwitValue<'_>, dictionary: &ByteDictionary) -> Result<JsonValue, String> {
    match value {
        QuickwitValue::Array(items) => items
            .iter()
            .map(|item| json_input(item, dictionary))
            .collect::<Result<Vec<_>, _>>()
            .map(JsonValue::Array),
        QuickwitValue::Object(object) => object
            .iter(dictionary)
            .map(|(k, v)| Ok((k.to_string(), json_input(v, dictionary)?)))
            .collect::<Result<serde_json::Map<_, _>, String>>()
            .map(JsonValue::Object),
        Leaf(QuickwitLeafValue::I64(number)) => Ok(number.into()),
        Leaf(QuickwitLeafValue::U64(number)) => Ok(number.into()),
        Leaf(QuickwitLeafValue::F64(number)) => serde_json::Number::from_f64(number)
            .map(JsonValue::Number)
            .ok_or_else(|| format!("expected a JSON number, got `{number}`")),
        Leaf(QuickwitLeafValue::String(id)) => Ok(dictionary.get_str(id).into()),
        Leaf(Null) => Ok(JsonValue::Null),
        Leaf(QuickwitLeafValue::Bool(boolean)) => Ok(boolean.into()),
        Leaf(
            QuickwitLeafValue::IpAddr(_)
            | QuickwitLeafValue::DateTime(_)
            | QuickwitLeafValue::Bytes(_),
        ) => unreachable!("parsed documents only contain JSON values"),
    }
}
