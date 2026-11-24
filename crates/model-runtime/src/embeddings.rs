//! The audited MP-03 text-only profile. Each batch is accepted or rejected as a whole.
use crate::ModelError;
use serde::{Deserialize, Serialize};
pub const PROFILE: &str = "multilingual-e5-small-onnx-cpu-v1";
pub const DIMENSIONS: usize = 384;
pub const MAX_BATCH: usize = 32;
pub const MAX_TOKENS_PER_TEXT: u32 = 512;

#[derive(Deserialize)]
#[serde(untagged)]
pub enum TextInput {
    Single(String),
    Batch(Vec<String>),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingRequest {
    model: String,
    input: TextInput,
    encoding_format: Option<String>,
    dimensions: Option<usize>,
}
impl EmbeddingRequest {
    pub fn into_texts(self) -> Result<Vec<String>, ModelError> {
        if self.model != PROFILE
            || self
                .encoding_format
                .as_deref()
                .is_some_and(|f| f != "float")
            || self.dimensions.is_some_and(|d| d != DIMENSIONS)
        {
            return Err(ModelError::Invalid);
        }
        let texts = match self.input {
            TextInput::Single(s) => vec![s],
            TextInput::Batch(b) => b,
        };
        validate_texts(&texts)?;
        let mut projected = Vec::with_capacity(texts.len());
        for text in texts {
            if let Some(content) = text
                .strip_prefix("query: ")
                .or_else(|| text.strip_prefix("passage: "))
            {
                if content.trim().is_empty() {
                    return Err(ModelError::Invalid);
                }
                projected.push(text);
            } else {
                projected.push(format!("query: {text}"));
            }
        }
        Ok(projected)
    }
}
pub fn validate_texts(texts: &[String]) -> Result<(), ModelError> {
    if texts.is_empty() || texts.iter().any(|t| t.trim().is_empty()) {
        return Err(ModelError::Invalid);
    }
    if texts.len() > MAX_BATCH
        || texts
            .iter()
            .try_fold(0usize, |sum, t| sum.checked_add(t.len()))
            .is_none_or(|n| n > 128 * 1024)
    {
        return Err(ModelError::Limit);
    }
    Ok(())
}
#[derive(Deserialize, Serialize)]
pub struct VectorRow {
    pub object: String,
    pub index: usize,
    pub embedding: Vec<f32>,
}
#[derive(Deserialize, Serialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}
#[derive(Deserialize, Serialize)]
pub struct BatchResult {
    pub model: String,
    pub object: String,
    pub data: Vec<VectorRow>,
    pub usage: EmbeddingUsage,
}
impl BatchResult {
    pub fn verify(&mut self, count: usize) -> Result<(), ModelError> {
        if self.model != PROFILE
            || self.object != "list"
            || !(1..=MAX_BATCH).contains(&count)
            || self.data.len() != count
            || !(count as u64..=count as u64 * u64::from(MAX_TOKENS_PER_TEXT))
                .contains(&self.usage.prompt_tokens)
            || self.usage.prompt_tokens != self.usage.total_tokens
        {
            return Err(ModelError::Protocol);
        }
        self.data.sort_unstable_by_key(|r| r.index);
        for (index, row) in self.data.iter().enumerate() {
            if row.object != "embedding" || row.index != index || row.embedding.len() != DIMENSIONS
            {
                return Err(ModelError::Protocol);
            }
            let squared: f64 = row.embedding.iter().map(|&x| f64::from(x).powi(2)).sum();
            if !squared.is_finite() || (squared - 1.0).abs() > 0.0001 {
                return Err(ModelError::Protocol);
            }
        }
        Ok(())
    }
    pub fn from_runner(
        vectors: Vec<Vec<f32>>,
        lengths: &[u64],
        count: usize,
    ) -> Result<Self, ModelError> {
        if lengths.len() != count
            || lengths
                .iter()
                .any(|n| !(1..=u64::from(MAX_TOKENS_PER_TEXT)).contains(n))
            || !(1..=MAX_BATCH).contains(&count)
        {
            return Err(ModelError::Protocol);
        }
        let tokens = lengths.iter().sum();
        let data = vectors
            .into_iter()
            .enumerate()
            .map(|(index, embedding)| VectorRow {
                object: "embedding".into(),
                index,
                embedding,
            })
            .collect();
        let mut result = Self {
            model: PROFILE.into(),
            object: "list".into(),
            data,
            usage: EmbeddingUsage {
                prompt_tokens: tokens,
                total_tokens: tokens,
            },
        };
        result.verify(count)?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vector(axis: usize) -> Vec<f32> {
        let mut x = vec![0.0; DIMENSIONS];
        x[axis] = 1.0;
        x
    }
    #[test]
    fn index_determines_row_order_and_duplicates_fail_the_batch() {
        let mut batch = BatchResult::from_runner(vec![vector(0), vector(1)], &[3, 7], 2)
            .expect("valid vectors");
        batch.data.swap(0, 1);
        batch.verify(2).expect("valid shuffled indices");
        assert_eq!(batch.data[0].embedding[0], 1.0);
        assert_eq!(batch.usage.prompt_tokens, 10);
        batch.data[1].index = 0;
        assert!(batch.verify(2).is_err());
    }
    #[test]
    fn dimension_finiteness_and_individual_limits_are_mandatory() {
        for bad in [vec![1.0], vec![0.0; DIMENSIONS], vec![f32::NAN; DIMENSIONS]] {
            assert!(BatchResult::from_runner(vec![vector(0), bad], &[2, 3], 2).is_err());
        }
        assert!(BatchResult::from_runner(vec![vector(0), vector(1)], &[513, 1], 2).is_err());
        assert!(BatchResult::from_runner(vec![vector(0)], &[3, 7], 2).is_err());
    }
}
