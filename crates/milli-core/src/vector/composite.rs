use std::time::Instant;

use arroy::Distance;

use super::error::CompositeEmbedderContainsHuggingFace;
use super::{
    hf, manual, ollama, openai, rest, DistributionShift, EmbedError, Embedding, EmbeddingCache,
    NewEmbedderError,
};
use crate::ThreadPoolNoAbort;

#[derive(Debug)]
pub enum SubEmbedder {
    /// An embedder based on running local models, fetched from the Hugging Face Hub.
    HuggingFace(hf::Embedder),
    /// An embedder based on making embedding queries against the OpenAI API.
    OpenAi(openai::Embedder),
    /// An embedder based on the user providing the embeddings in the documents and queries.
    UserProvided(manual::Embedder),
    /// An embedder based on making embedding queries against an <https://ollama.com> embedding server.
    Ollama(ollama::Embedder),
    /// An embedder based on making embedding queries against a generic JSON/REST embedding server.
    Rest(rest::Embedder),
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub enum SubEmbedderOptions {
    HuggingFace(hf::EmbedderOptions),
    OpenAi(openai::EmbedderOptions),
    Ollama(ollama::EmbedderOptions),
    UserProvided(manual::EmbedderOptions),
    Rest(rest::EmbedderOptions),
}

impl SubEmbedderOptions {
    pub fn distribution(&self) -> Option<DistributionShift> {
        match self {
            SubEmbedderOptions::HuggingFace(embedder_options) => embedder_options.distribution,
            SubEmbedderOptions::OpenAi(embedder_options) => embedder_options.distribution,
            SubEmbedderOptions::Ollama(embedder_options) => embedder_options.distribution,
            SubEmbedderOptions::UserProvided(embedder_options) => embedder_options.distribution,
            SubEmbedderOptions::Rest(embedder_options) => embedder_options.distribution,
        }
    }
}

#[derive(Debug)]
pub struct Embedder {
    pub(super) search: SubEmbedder,
    pub(super) index: SubEmbedder,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct EmbedderOptions {
    pub search: SubEmbedderOptions,
    pub index: SubEmbedderOptions,
}

impl Embedder {
    pub fn new(
        EmbedderOptions { search, index }: EmbedderOptions,
        cache_cap: usize,
    ) -> Result<Self, NewEmbedderError> {
        let search = SubEmbedder::new(search, cache_cap)?;
        // cache is only used at search
        let index = SubEmbedder::new(index, 0)?;

        // check dimensions
        if search.dimensions() != index.dimensions() {
            return Err(NewEmbedderError::composite_dimensions_mismatch(
                search.dimensions(),
                index.dimensions(),
            ));
        }
        // check similarity
        let search_embeddings = search
            .embed(
                vec![
                    "test".into(),
                    "a brave dog".into(),
                    "This is a sample text. It is meant to compare similarity.".into(),
                ],
                None,
            )
            .map_err(|error| NewEmbedderError::composite_test_embedding_failed(error, "search"))?;

        let index_embeddings = index
            .embed(
                vec![
                    "test".into(),
                    "a brave dog".into(),
                    "This is a sample text. It is meant to compare similarity.".into(),
                ],
                None,
            )
            .map_err(|error| {
                NewEmbedderError::composite_test_embedding_failed(error, "indexing")
            })?;

        let hint = configuration_hint(&search, &index);

        check_similarity(search_embeddings, index_embeddings, hint)?;

        Ok(Self { search, index })
    }

    /// Indicates the dimensions of a single embedding produced by the embedder.
    pub fn dimensions(&self) -> usize {
        // can use the dimensions of any embedder since they should match
        self.index.dimensions()
    }

    /// An optional distribution used to apply an affine transformation to the similarity score of a document.
    pub fn distribution(&self) -> Option<DistributionShift> {
        // 3 cases here:
        // 1. distribution provided by user => use that one, which was stored in search
        // 2. no user-provided distribution, distribution in search embedder => use that one
        // 2. no user-provided distribution, no distribution in search embedder => use the distribution in indexing embedder
        self.search.distribution().or_else(|| self.index.distribution())
    }
}

impl SubEmbedder {
    pub fn new(
        options: SubEmbedderOptions,
        cache_cap: usize,
    ) -> std::result::Result<Self, NewEmbedderError> {
        Ok(match options {
            SubEmbedderOptions::HuggingFace(options) => {
                Self::HuggingFace(hf::Embedder::new(options, cache_cap)?)
            }
            SubEmbedderOptions::OpenAi(options) => {
                Self::OpenAi(openai::Embedder::new(options, cache_cap)?)
            }
            SubEmbedderOptions::Ollama(options) => {
                Self::Ollama(ollama::Embedder::new(options, cache_cap)?)
            }
            SubEmbedderOptions::UserProvided(options) => {
                Self::UserProvided(manual::Embedder::new(options))
            }
            SubEmbedderOptions::Rest(options) => Self::Rest(rest::Embedder::new(
                options,
                cache_cap,
                rest::ConfigurationSource::User,
            )?),
        })
    }

    pub fn embed(
        &self,
        texts: Vec<String>,
        deadline: Option<Instant>,
    ) -> std::result::Result<Vec<Embedding>, EmbedError> {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.embed(texts),
            SubEmbedder::OpenAi(embedder) => embedder.embed(&texts, deadline),
            SubEmbedder::Ollama(embedder) => embedder.embed(&texts, deadline),
            SubEmbedder::UserProvided(embedder) => embedder.embed(&texts),
            SubEmbedder::Rest(embedder) => embedder.embed(texts, deadline),
        }
    }

    pub fn embed_one(
        &self,
        text: &str,
        deadline: Option<Instant>,
    ) -> std::result::Result<Embedding, EmbedError> {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.embed_one(text),
            SubEmbedder::OpenAi(embedder) => {
                embedder.embed(&[text], deadline)?.pop().ok_or_else(EmbedError::missing_embedding)
            }
            SubEmbedder::Ollama(embedder) => {
                embedder.embed(&[text], deadline)?.pop().ok_or_else(EmbedError::missing_embedding)
            }
            SubEmbedder::UserProvided(embedder) => embedder.embed_one(text),
            SubEmbedder::Rest(embedder) => embedder
                .embed_ref(&[text], deadline)?
                .pop()
                .ok_or_else(EmbedError::missing_embedding),
        }
    }

    /// Embed multiple chunks of texts.
    ///
    /// Each chunk is composed of one or multiple texts.
    pub fn embed_index(
        &self,
        text_chunks: Vec<Vec<String>>,
        threads: &ThreadPoolNoAbort,
    ) -> std::result::Result<Vec<Vec<Embedding>>, EmbedError> {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.embed_index(text_chunks),
            SubEmbedder::OpenAi(embedder) => embedder.embed_index(text_chunks, threads),
            SubEmbedder::Ollama(embedder) => embedder.embed_index(text_chunks, threads),
            SubEmbedder::UserProvided(embedder) => embedder.embed_index(text_chunks),
            SubEmbedder::Rest(embedder) => embedder.embed_index(text_chunks, threads),
        }
    }

    /// Non-owning variant of [`Self::embed_index`].
    pub fn embed_index_ref(
        &self,
        texts: &[&str],
        threads: &ThreadPoolNoAbort,
    ) -> std::result::Result<Vec<Embedding>, EmbedError> {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.embed_index_ref(texts),
            SubEmbedder::OpenAi(embedder) => embedder.embed_index_ref(texts, threads),
            SubEmbedder::Ollama(embedder) => embedder.embed_index_ref(texts, threads),
            SubEmbedder::UserProvided(embedder) => embedder.embed_index_ref(texts),
            SubEmbedder::Rest(embedder) => embedder.embed_index_ref(texts, threads),
        }
    }

    /// Indicates the preferred number of chunks to pass to [`Self::embed_chunks`]
    pub fn chunk_count_hint(&self) -> usize {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.chunk_count_hint(),
            SubEmbedder::OpenAi(embedder) => embedder.chunk_count_hint(),
            SubEmbedder::Ollama(embedder) => embedder.chunk_count_hint(),
            SubEmbedder::UserProvided(_) => 100,
            SubEmbedder::Rest(embedder) => embedder.chunk_count_hint(),
        }
    }

    /// Indicates the preferred number of texts in a single chunk passed to [`Self::embed`]
    pub fn prompt_count_in_chunk_hint(&self) -> usize {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.prompt_count_in_chunk_hint(),
            SubEmbedder::OpenAi(embedder) => embedder.prompt_count_in_chunk_hint(),
            SubEmbedder::Ollama(embedder) => embedder.prompt_count_in_chunk_hint(),
            SubEmbedder::UserProvided(_) => 1,
            SubEmbedder::Rest(embedder) => embedder.prompt_count_in_chunk_hint(),
        }
    }

    pub fn uses_document_template(&self) -> bool {
        match self {
            SubEmbedder::HuggingFace(_)
            | SubEmbedder::OpenAi(_)
            | SubEmbedder::Ollama(_)
            | SubEmbedder::Rest(_) => true,
            SubEmbedder::UserProvided(_) => false,
        }
    }

    /// Indicates the dimensions of a single embedding produced by the embedder.
    pub fn dimensions(&self) -> usize {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.dimensions(),
            SubEmbedder::OpenAi(embedder) => embedder.dimensions(),
            SubEmbedder::Ollama(embedder) => embedder.dimensions(),
            SubEmbedder::UserProvided(embedder) => embedder.dimensions(),
            SubEmbedder::Rest(embedder) => embedder.dimensions(),
        }
    }

    /// An optional distribution used to apply an affine transformation to the similarity score of a document.
    pub fn distribution(&self) -> Option<DistributionShift> {
        match self {
            SubEmbedder::HuggingFace(embedder) => embedder.distribution(),
            SubEmbedder::OpenAi(embedder) => embedder.distribution(),
            SubEmbedder::Ollama(embedder) => embedder.distribution(),
            SubEmbedder::UserProvided(embedder) => embedder.distribution(),
            SubEmbedder::Rest(embedder) => embedder.distribution(),
        }
    }

    pub(super) fn cache(&self) -> Option<&EmbeddingCache> {
        match self {
            SubEmbedder::HuggingFace(embedder) => Some(embedder.cache()),
            SubEmbedder::OpenAi(embedder) => Some(embedder.cache()),
            SubEmbedder::UserProvided(_) => None,
            SubEmbedder::Ollama(embedder) => Some(embedder.cache()),
            SubEmbedder::Rest(embedder) => Some(embedder.cache()),
        }
    }
}

fn check_similarity(
    left: Vec<Embedding>,
    right: Vec<Embedding>,
    hint: CompositeEmbedderContainsHuggingFace,
) -> Result<(), NewEmbedderError> {
    if left.len() != right.len() {
        return Err(NewEmbedderError::composite_embedding_count_mismatch(left.len(), right.len()));
    }

    for (left, right) in left.into_iter().zip(right) {
        let left = arroy::internals::UnalignedVector::from_slice(&left);
        let right = arroy::internals::UnalignedVector::from_slice(&right);
        let left = arroy::internals::Leaf {
            header: arroy::distances::Cosine::new_header(&left),
            vector: left,
        };
        let right = arroy::internals::Leaf {
            header: arroy::distances::Cosine::new_header(&right),
            vector: right,
        };

        let distance = arroy::distances::Cosine::built_distance(&left, &right);

        if distance > super::MAX_COMPOSITE_DISTANCE {
            return Err(NewEmbedderError::composite_embedding_value_mismatch(distance, hint));
        }
    }
    Ok(())
}

fn configuration_hint(
    search: &SubEmbedder,
    index: &SubEmbedder,
) -> CompositeEmbedderContainsHuggingFace {
    match (search, index) {
        (SubEmbedder::HuggingFace(_), SubEmbedder::HuggingFace(_)) => {
            CompositeEmbedderContainsHuggingFace::Both
        }
        (SubEmbedder::HuggingFace(_), _) => CompositeEmbedderContainsHuggingFace::Search,
        (_, SubEmbedder::HuggingFace(_)) => CompositeEmbedderContainsHuggingFace::Indexing,
        _ => CompositeEmbedderContainsHuggingFace::None,
    }
}
