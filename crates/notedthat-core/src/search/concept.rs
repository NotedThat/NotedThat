//! Open Knowledge Format metadata attached to search hits.

use serde::{Deserialize, Serialize};

/// Metadata identifying the OKF concept containing a search chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConceptMetadata {
    /// Bundle-relative document path without the `.md` extension.
    pub concept_id: String,
    /// OKF concept type, including custom types.
    #[serde(rename = "type")]
    pub concept_type: String,
    /// Human-readable concept title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Short concept description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Resource identifier associated with the concept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
    /// Tags used to discover the concept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::ConceptMetadata;

    #[test]
    fn minimal_metadata_round_trips_with_okf_field_names() {
        let json = serde_json::json!({"concept_id": "revenue", "type": "business-glossary"});
        let metadata: ConceptMetadata = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(metadata.concept_type, "business-glossary");
        assert_eq!(serde_json::to_value(metadata).unwrap(), json);
    }
}
