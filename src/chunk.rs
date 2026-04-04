use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub display_text: String,
    pub embedding_text: String,
    pub metadata: ChunkMetadata,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkMetadata {
    pub symbol_name: String,
    pub kind: ChunkKind,
    pub chunk_type: ChunkType,
    pub parent: Option<String>,
    pub visibility: Visibility,
    pub file_path: String,
    pub line_range: (usize, usize),
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkKind {
    Function,
    Struct,
    Enum,
    Trait,
    Impl,
    Module,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChunkType {
    Overview,
    Function,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Pub,
    PubCrate,
    PubSuper,
    Private,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_bincode_round_trip() {
        let original = Chunk {
            display_text: "fn foo() {}".to_string(),
            embedding_text: "function foo".to_string(),
            metadata: ChunkMetadata {
                symbol_name: "foo".to_string(),
                kind: ChunkKind::Function,
                chunk_type: ChunkType::Function,
                parent: Some("bar".to_string()),
                visibility: Visibility::Pub,
                file_path: "src/lib.rs".to_string(),
                line_range: (1, 3),
                signature: Some("fn foo()".to_string()),
            },
            embedding: Some(vec![0.1, 0.2, 0.3]),
        };

        let encoded = bincode::serialize(&original).expect("serialize failed");
        let decoded: Chunk = bincode::deserialize(&encoded).expect("deserialize failed");
        assert_eq!(original, decoded);
    }
}
