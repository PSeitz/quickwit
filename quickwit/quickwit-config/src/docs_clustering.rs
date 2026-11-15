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

use std::collections::HashMap;

use anyhow::ensure;
use serde::{Deserialize, Deserializer, Serialize};

use crate::config_value::ConfigValue;
use crate::qw_env_vars::QW_DISABLE_DOCS_CLUSTERING;

/// Experimental document clustering for fresh splits, using ordered string field paths.
///
/// YAML uses a flat list of individual fields:
/// ```yaml
/// - kind: raw
///   path: service
/// - kind: raw
///   path: status
/// ```
/// Only string values participate. Missing and nonstring values are ignored. For repeated values
/// and arrays, only the first value is considered. Structure and combined policies are removed.
/// Tokenized clustering is unsupported.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    try_from = "DocsClusteringConfigBuilder",
    into = "DocsClusteringConfigBuilder"
)]
pub struct DocsClusteringConfig {
    /// Ordered paths resolved against the mapped Tantivy schema and JSON fields by the indexer.
    pub fields: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct DocsClusteringConfigBuilder {
    fields: Vec<ClusteringField>,
}

/// Serialized field entry. The runtime config carries only its path.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ClusteringField {
    kind: ClusteringKind,
    path: String,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ClusteringKind {
    Raw,
}

impl<'de> Deserialize<'de> for ClusteringKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        let kind = String::deserialize(deserializer)?;
        match kind.as_str() {
            "raw" => Ok(Self::Raw),
            "tokenized" => Err(serde::de::Error::custom(
                "tokenized document clustering is unsupported",
            )),
            _ => Err(serde::de::Error::unknown_variant(&kind, &["raw"])),
        }
    }
}

impl DocsClusteringConfigBuilder {
    pub(crate) fn build_optional(
        config_builder_opt: Option<Self>,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<Option<DocsClusteringConfig>> {
        let disable_override =
            ConfigValue::<bool, QW_DISABLE_DOCS_CLUSTERING>::none().resolve_optional(env_vars)?;

        let Some(config_builder) = config_builder_opt else {
            return Ok(None);
        };
        // Preserve the override's bypass of path validation for supported raw entries.
        // Unsupported kinds and malformed entries already fail during deserialization.
        if disable_override == Some(true) {
            return Ok(None);
        }
        Ok(Some(config_builder.try_into()?))
    }
}

impl TryFrom<DocsClusteringConfigBuilder> for DocsClusteringConfig {
    type Error = anyhow::Error;

    fn try_from(builder: DocsClusteringConfigBuilder) -> anyhow::Result<Self> {
        let config = Self {
            fields: builder.fields.into_iter().map(|field| field.path).collect(),
        };
        config.validate()?;
        Ok(config)
    }
}

impl From<DocsClusteringConfig> for DocsClusteringConfigBuilder {
    fn from(config: DocsClusteringConfig) -> Self {
        Self {
            fields: config
                .fields
                .into_iter()
                .map(|path| ClusteringField {
                    kind: ClusteringKind::Raw,
                    path,
                })
                .collect(),
        }
    }
}

impl DocsClusteringConfig {
    /// Validates the field list and path syntax, including for programmatically constructed
    /// configs. The indexer additionally checks whether paths resolve in each index's mapped
    /// schema.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.fields.is_empty(),
            "document clustering fields must contain at least one field"
        );
        for path in &self.fields {
            ensure!(
                !path.split('.').any(str::is_empty),
                "document clustering path `{path}` must not contain empty components"
            );
            ensure!(
                !path
                    .split('.')
                    .any(|component| component.trim() != component),
                "document clustering path `{path}` must not contain leading or trailing whitespace"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{DocsClusteringConfig, DocsClusteringConfigBuilder};

    const RAW_FIELDS: &str = "[{kind: raw, path: service}, {kind: raw, path: attributes.status}]";

    fn build_config(
        yaml: &str,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<Option<DocsClusteringConfig>> {
        let builder = serde_yaml::from_str::<DocsClusteringConfigBuilder>(yaml)?;
        DocsClusteringConfigBuilder::build_optional(Some(builder), env_vars)
    }

    #[test]
    fn raw_fields_preserve_order_and_round_trip() {
        let config = build_config(RAW_FIELDS, &HashMap::new()).unwrap().unwrap();
        assert_eq!(config.fields, ["service", "attributes.status"]);
        let expected = serde_json::json!([
            {"kind": "raw", "path": "service"},
            {"kind": "raw", "path": "attributes.status"}
        ]);
        assert_eq!(serde_json::to_value(&config).unwrap(), expected);
        assert_eq!(
            serde_json::from_value::<DocsClusteringConfig>(expected).unwrap(),
            config
        );
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert_eq!(
            serde_yaml::from_str::<DocsClusteringConfig>(&yaml).unwrap(),
            config
        );
    }

    #[test]
    fn validation_rejects_empty_fields_and_invalid_paths() {
        let invalid_configs = [
            (vec![], "must contain at least one field"),
            (vec![""], "must not contain empty components"),
            (vec![".service"], "must not contain empty components"),
            (vec!["service."], "must not contain empty components"),
            (
                vec!["attributes..status"],
                "must not contain empty components",
            ),
            (
                vec![" service"],
                "must not contain leading or trailing whitespace",
            ),
            (
                vec!["attributes. status"],
                "must not contain leading or trailing whitespace",
            ),
            (
                vec!["attributes.status "],
                "must not contain leading or trailing whitespace",
            ),
        ];
        for (paths, expected_error) in invalid_configs {
            let config = DocsClusteringConfig {
                fields: paths.into_iter().map(str::to_string).collect(),
            };
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .to_string()
                    .contains(expected_error)
            );
            let yaml = serde_yaml::to_string(&config).unwrap();
            let error = serde_yaml::from_str::<DocsClusteringConfig>(&yaml).unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error}");
            let json = serde_json::to_value(&config).unwrap();
            let error = serde_json::from_value::<DocsClusteringConfig>(json).unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error}");
            let error = build_config(&yaml, &HashMap::new()).unwrap_err();
            assert!(error.to_string().contains(expected_error), "{error}");
        }
    }

    #[test]
    fn deserialization_rejects_removed_and_malformed_entries() {
        for yaml in [
            "[{kind: structure}]",
            "[{fingerprint: [{kind: raw, path: service}]}]",
            "[{kind: combined, fields: [service, status]}]",
            "[{kind: raw, path: service, extra: nope}]",
            "[{kind: raw}]",
            "[{path: service}]",
            "[{kind: templated, path: message}]",
            "[{kind: raw, path: [service, status]}]",
            "{fields: [service]}",
            "[",
        ] {
            assert!(
                serde_yaml::from_str::<DocsClusteringConfig>(yaml).is_err(),
                "{yaml}"
            );
            assert!(
                serde_yaml::from_str::<DocsClusteringConfigBuilder>(yaml).is_err(),
                "{yaml}"
            );
        }
    }

    #[test]
    fn tokenized_is_explicitly_unsupported_even_when_disabled() {
        let env_vars = HashMap::from([("QW_DISABLE_DOCS_CLUSTERING".into(), "true".into())]);
        for yaml in [
            "[{kind: tokenized, path: message}]",
            "[{kind: tokenized, path: message, max_tokens: 50}]",
        ] {
            let error = serde_yaml::from_str::<DocsClusteringConfig>(yaml).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("tokenized document clustering is unsupported")
            );
            let error = build_config(yaml, &env_vars).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("tokenized document clustering is unsupported")
            );
        }
    }

    #[test]
    fn disable_override_preserves_supported_config_behavior() {
        let disabled = HashMap::from([("QW_DISABLE_DOCS_CLUSTERING".into(), "true".into())]);
        assert!(build_config(RAW_FIELDS, &disabled).unwrap().is_none());
        for yaml in ["[]", "[{kind: raw, path: attributes..status}]"] {
            assert!(build_config(yaml, &disabled).unwrap().is_none());
        }
        let enabled = HashMap::from([("QW_DISABLE_DOCS_CLUSTERING".into(), "false".into())]);
        assert_eq!(
            build_config(RAW_FIELDS, &enabled).unwrap(),
            build_config(RAW_FIELDS, &HashMap::new()).unwrap()
        );
        for env_vars in [&enabled, &disabled, &HashMap::new()] {
            assert!(
                DocsClusteringConfigBuilder::build_optional(None, env_vars)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn invalid_disable_override_is_an_error_even_without_config() {
        let env_vars = HashMap::from([("QW_DISABLE_DOCS_CLUSTERING".into(), "invalid".into())]);
        let error = DocsClusteringConfigBuilder::build_optional(None, &env_vars).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("failed to convert value `invalid`")
        );
        assert!(build_config(RAW_FIELDS, &env_vars).is_err());
    }
}
