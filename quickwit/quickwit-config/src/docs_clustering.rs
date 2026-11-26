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
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config_value::ConfigValue;
use crate::qw_env_vars::QW_DISABLE_DOCS_CLUSTERING;

/// Experimental document clustering for fresh splits.
///
/// YAML uses an ordered list of grouping levels, from outermost to innermost:
/// ```yaml
/// - kind: raw
///   path: service
/// - kind: structure
/// - kind: raw
///   path: status
/// ```
/// Field levels group by string values at mapped schema or JSON paths (arrays and repeated fields
/// use the first value; missing and non-string values share an ignored bucket before string
/// groups). Structure levels group by a 64-bit hash of the document's unique mapped leaf paths.
/// Tokenized clustering is unsupported.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    try_from = "DocsClusteringConfigBuilder",
    into = "DocsClusteringConfigBuilder"
)]
pub struct DocsClusteringConfig {
    /// Ordered grouping keys, from outermost to innermost.
    pub group_by: Vec<ClusteringKey>,
}

/// How to group documents at one level of clustering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClusteringKey {
    /// Group by a mapped schema field or JSON path (string values).
    Field(String),
    /// Group by a 64-bit hash of the document's unique mapped leaf paths.
    Structure,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct DocsClusteringConfigBuilder {
    group_by: Vec<ClusteringKeyEntry>,
}

#[derive(Debug, PartialEq)]
enum ClusteringKeyEntry {
    Raw { path: String },
    Structure,
}

impl Serialize for ClusteringKeyEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Tagged<'a> {
            Raw { path: &'a str },
            Structure,
        }
        match self {
            Self::Raw { path } => Tagged::Raw { path }.serialize(serializer),
            Self::Structure => Tagged::Structure.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ClusteringKeyEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: String,
            path: Option<String>,
            max_tokens: Option<usize>,
        }
        let wire = Wire::deserialize(deserializer)?;
        match wire.kind.as_str() {
            "raw" => {
                if wire.max_tokens.is_some() {
                    return Err(serde::de::Error::unknown_field("max_tokens", &["path"]));
                }
                let path = wire
                    .path
                    .ok_or_else(|| serde::de::Error::missing_field("path"))?;
                Ok(Self::Raw { path })
            }
            "structure" => {
                if wire.path.is_some() {
                    return Err(serde::de::Error::unknown_field("path", &["kind"]));
                }
                if wire.max_tokens.is_some() {
                    return Err(serde::de::Error::unknown_field("max_tokens", &["kind"]));
                }
                Ok(Self::Structure)
            }
            "tokenized" => Err(serde::de::Error::custom(
                "tokenized document clustering is unsupported",
            )),
            other => Err(serde::de::Error::unknown_variant(
                other,
                &["raw", "structure"],
            )),
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
        // Preserve the override's bypass of path validation for supported entries.
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
            group_by: builder
                .group_by
                .into_iter()
                .map(|entry| match entry {
                    ClusteringKeyEntry::Raw { path } => ClusteringKey::Field(path),
                    ClusteringKeyEntry::Structure => ClusteringKey::Structure,
                })
                .collect(),
        };
        config.validate()?;
        Ok(config)
    }
}

impl From<DocsClusteringConfig> for DocsClusteringConfigBuilder {
    fn from(config: DocsClusteringConfig) -> Self {
        Self {
            group_by: config
                .group_by
                .into_iter()
                .map(|key| match key {
                    ClusteringKey::Field(path) => ClusteringKeyEntry::Raw { path },
                    ClusteringKey::Structure => ClusteringKeyEntry::Structure,
                })
                .collect(),
        }
    }
}

impl DocsClusteringConfig {
    /// Validates the grouping keys and path syntax, including for programmatically constructed
    /// configs. The indexer additionally checks whether field paths resolve in each index's mapped
    /// schema.
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            !self.group_by.is_empty(),
            "document clustering keys must contain at least one entry"
        );
        for key in &self.group_by {
            let ClusteringKey::Field(path) = key else {
                continue;
            };
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

    use super::{ClusteringKey, DocsClusteringConfig, DocsClusteringConfigBuilder};

    const SAMPLE_KEYS: &str =
        "[{kind: raw, path: service}, {kind: structure}, {kind: raw, path: attributes.status}]";

    fn build_config(
        yaml: &str,
        env_vars: &HashMap<String, String>,
    ) -> anyhow::Result<Option<DocsClusteringConfig>> {
        let builder = serde_yaml::from_str::<DocsClusteringConfigBuilder>(yaml)?;
        DocsClusteringConfigBuilder::build_optional(Some(builder), env_vars)
    }

    #[test]
    fn keys_preserve_order_and_round_trip() {
        let config = build_config(SAMPLE_KEYS, &HashMap::new()).unwrap().unwrap();
        assert_eq!(
            config.group_by,
            [
                ClusteringKey::Field("service".into()),
                ClusteringKey::Structure,
                ClusteringKey::Field("attributes.status".into()),
            ]
        );
        let expected = serde_json::json!([
            {"kind": "raw", "path": "service"},
            {"kind": "structure"},
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
    fn validation_rejects_empty_keys_and_invalid_paths() {
        let invalid_configs = [
            (vec![], "must contain at least one entry"),
            (
                vec![ClusteringKey::Field("".into())],
                "must not contain empty components",
            ),
            (
                vec![ClusteringKey::Field(".service".into())],
                "must not contain empty components",
            ),
            (
                vec![ClusteringKey::Field("service.".into())],
                "must not contain empty components",
            ),
            (
                vec![ClusteringKey::Field("attributes..status".into())],
                "must not contain empty components",
            ),
            (
                vec![ClusteringKey::Field(" service".into())],
                "must not contain leading or trailing whitespace",
            ),
            (
                vec![ClusteringKey::Field("attributes. status".into())],
                "must not contain leading or trailing whitespace",
            ),
            (
                vec![ClusteringKey::Field("attributes.status ".into())],
                "must not contain leading or trailing whitespace",
            ),
        ];
        for (group_by, expected_error) in invalid_configs {
            let config = DocsClusteringConfig { group_by };
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
            "[{fingerprint: [{kind: raw, path: service}]}]",
            "[{kind: structure, exclude: [custom]}]",
            "[{kind: structure, path: service}]",
            "[{kind: combined, fields: [service, status]}]",
            "[{kind: raw, path: service, extra: nope}]",
            "[{kind: raw, path: service, max_tokens: 10}]",
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
        assert!(build_config(SAMPLE_KEYS, &disabled).unwrap().is_none());
        for yaml in ["[]", "[{kind: raw, path: attributes..status}]"] {
            assert!(build_config(yaml, &disabled).unwrap().is_none());
        }
        let enabled = HashMap::from([("QW_DISABLE_DOCS_CLUSTERING".into(), "false".into())]);
        assert_eq!(
            build_config(SAMPLE_KEYS, &enabled).unwrap(),
            build_config(SAMPLE_KEYS, &HashMap::new()).unwrap()
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
        assert!(build_config(SAMPLE_KEYS, &env_vars).is_err());
    }
}
