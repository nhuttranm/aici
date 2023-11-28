use std::fmt::Debug;

use candle::Tensor;
use serde::{Deserialize, Serialize};

use crate::{blocks::BlockRef, config::SamplingParams, LogitsProcessor};

pub type Token = u32;
pub type SeqId = u32;

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum FinishReason {
    /// EOS token was generated.
    FoundEos,
    /// SamplingParams.max_tokens reached.
    MaxTokensReached,
    /// Explicit abort request on the engine.
    Aborted,
    /// The scheduler didn't like the sequence.
    Failed,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SchedulingPhase {
    Waiting,
    Running,
    Swapped,
    Finished(FinishReason),
}

#[derive(Debug, Clone, Copy)]
pub enum StepType {
    Prompt,
    Fixed(usize),
    Gen,
}

pub struct Sequence {
    pub seq_id: SeqId,
    pub step_type: StepType,
    pub tokens: Vec<Token>,
    pub prompt_len: usize,

    // state for Scheduler and BlockSpaceManager
    pub(crate) sched_phase: SchedulingPhase,
    pub(crate) gpu_blocks: Vec<BlockRef>,
    pub(crate) cpu_blocks: Vec<BlockRef>,
    block_size: usize,
}

impl Debug for Sequence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sequence")
            .field("seq_id", &self.seq_id)
            .field("sched_phase", &self.sched_phase)
            .field("step_type", &self.step_type)
            .field("tokens", &self.tokens)
            .field("prompt_len", &self.prompt_len)
            .finish()
    }
}

impl Sequence {
    pub(crate) fn new(seq_id: SeqId, tokens: &[Token], block_size: usize) -> Self {
        let prompt_len = tokens.len();
        let mut seq = Self {
            seq_id,
            sched_phase: SchedulingPhase::Waiting,
            step_type: StepType::Prompt,
            tokens: Vec::new(),
            prompt_len,
            gpu_blocks: Vec::new(),
            cpu_blocks: Vec::new(),
            block_size,
        };
        seq._append_tokens_to_blocks(tokens);
        seq
    }

    pub fn get_len(&self) -> usize {
        self.tokens.len()
    }

    pub fn get_gen_len(&self) -> usize {
        self.tokens.len() - self.prompt_len
    }

    pub fn get_gpu_slot(&self, position: usize) -> usize {
        let block_index = self.gpu_blocks[position / self.block_size].get_index();
        let block_offset = position % self.block_size;
        block_index * self.block_size + block_offset
    }

    #[allow(dead_code)]
    pub(crate) fn fork_as(&self, seq_id: SeqId) -> Self {
        let mut seq = Self {
            seq_id,
            sched_phase: self.sched_phase,
            step_type: self.step_type,
            tokens: self.tokens.clone(),
            prompt_len: self.prompt_len,
            gpu_blocks: self.gpu_blocks.iter().map(|x| x.fork()).collect(),
            cpu_blocks: self.cpu_blocks.iter().map(|x| x.fork()).collect(),
            block_size: self.block_size,
        };
        seq._append_tokens_to_blocks(&self.tokens);
        seq
    }

    fn _append_tokens_to_blocks(&mut self, token_ids: &[Token]) {
        self.tokens.extend_from_slice(token_ids);
    }

    pub fn append_token_id(&mut self, token_id: Token) {
        self._append_tokens_to_blocks(&[token_id]);
    }

    pub fn finish_reason(&self) -> Option<FinishReason> {
        match self.sched_phase {
            SchedulingPhase::Finished(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn get_output(&self) -> SeqOutput {
        SeqOutput {
            seq_id: self.seq_id,
            output_tokens: self.tokens[self.prompt_len..].to_vec(),
            finish_reason: self.finish_reason(),
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finish_reason().is_some()
    }
}

/// A group of sequences that are generated from the same prompt.
pub struct SequenceGroup {
    pub request_id: String,
    pub seqs: Vec<Sequence>,
    pub sampling_params: SamplingParams,
    pub arrival_time: std::time::Instant,
    pub logits_processor: LogitsProcessor,
}

#[derive(Debug)]
pub struct BatchInfo {
    pub tokens: Tensor,         // u32, [num_tokens]
    pub positions: Tensor,      // i64, [num_tokens]
    pub seqlens_q: Tensor,      // u32, [batch_size + 1]; points to tokens/positions
    pub seqlens_k: Tensor,      // u32, [batch_size + 1]; can go outside tokens/positions
    pub gather_mapping: Tensor, // u32, [sum(context_len + prompt_len)]
    pub slot_mapping: Tensor,   // u32, [num_tokens]
    pub max_seqlen_q: usize,
    pub max_seqlen_k: usize,
    pub kv_cache: Vec<(Tensor, Tensor)>,
}

impl SequenceGroup {
    /// The maximum number of sequences running in parallel in the remaining
    /// lifetime of the request.
    pub fn get_max_num_running_seqs(&self) -> usize {
        if self.sampling_params.use_beam_search {
            // For beam search, maximally there will always be `best_of` beam
            // candidates running in the future.
            self.sampling_params.best_of
        } else {
            if self.sampling_params.best_of > self.num_seqs(None) {
                // At prompt stage, the sequence group is not yet filled up
                // and only have one sequence running. However, in the
                // generation stage, we will have `best_of` sequences running.
                self.sampling_params.best_of
            } else {
                // At sampling stages, return the number of actual sequences
                // running.
                self.num_seqs(Some(SchedulingPhase::Running))
            }
        }
    }

    pub fn only_seq(&self) -> &Sequence {
        if self.seqs.len() == 1 {
            &self.seqs[0]
        } else {
            panic!("num seq {} != 1", self.seqs.len());
        }
    }

    /// Retrieves sequences, optionally filtered by status.
    pub fn get_seqs(&self, status: Option<SchedulingPhase>) -> Vec<&Sequence> {
        match status {
            Some(status_filter) => self
                .seqs
                .iter()
                .filter(|seq| seq.sched_phase == status_filter)
                .collect(),
            None => self.seqs.iter().collect(),
        }
    }

    /// Returns the number of sequences, optionally filtered by status.
    pub fn num_seqs(&self, status: Option<SchedulingPhase>) -> usize {
        self.get_seqs(status).len()
    }

    /// Adds a sequence.
    #[allow(dead_code)]
    pub(crate) fn add(&mut self, seq: Sequence) {
        self.seqs.push(seq)
    }

    /// Checks if all sequences are finished.
    pub fn is_finished(&self) -> bool {
        self.seqs.iter().all(|seq| seq.is_finished())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeqOutput {
    pub seq_id: SeqId,
    /// The tokens generated by the model. Doesn't include prompt tokens.
    pub output_tokens: Vec<Token>,
    pub finish_reason: Option<FinishReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestOutput {
    pub request_id: String,
    pub seq_outputs: Vec<SeqOutput>,
}

/*
You are PyRust Translator, designed to assist users in translating Python code into Rust.
- only translate code, do not explain differences between Python and Rust
- if Python code is using the 'pytorch' package, the Rust should use 'candle' (assuming similar APIs to 'tch' and 'pytorch')
- keep comments and docstrings; attach docstrings to struct fields or parameters as appropriate in Rust
- keep asserts
- provide complete translations, filling out all methods and their bodies; avoid comments like "// Similar to Python" or "// Implement other methods"
- always translate code, even if it won't work to provide a base line for the user

*/
