use super::{
    calculate_inputs, get_completion_input, get_model_paths, get_prompt_input, get_xlora_paths,
    Loader, ModelKind, ModelPaths, Pipeline, TokenSource, XLoraPaths,
};
use crate::models::Cache;
use crate::pipeline::ChatTemplate;
use crate::xlora_models::{XLoraConfig, XLoraGemma};
use crate::{deref_mut_refcell, deref_refcell, deserialize_chat_template};
use crate::{
    models::gemma::{Config, Model as NormalModel},
    sequence::Sequence,
    utils::{tokens::get_token, varbuilder_utils::from_mmaped_safetensors},
};
use anyhow::Result;
use candle_core::{DType, Device, Tensor};
use candle_sampling::logits_processor::Logprobs;
use either::Either;
use hf_hub::{api::sync::ApiBuilder, Repo, RepoType};
use mistralrs_lora::{LoraConfig, Ordering};
use serde::Deserialize;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::{rc::Rc, sync::Mutex};
use thiserror::Error;
use tokenizers::Tokenizer;

enum Model {
    Normal(NormalModel),
    XLoraNormal(XLoraGemma),
}

pub struct GemmaModelPaths<P> {
    tokenizer_filename: P,
    config_filename: P,
    template_filename: P,
    filenames: Vec<P>,
    xlora_adapter_filenames: Option<Vec<(String, P)>>,
    xlora_adapter_configs: Option<Vec<(String, LoraConfig)>>,
    classifier_path: Option<P>,
    classifier_config: Option<XLoraConfig>,
    xlora_ordering: Option<Ordering>,
}

impl ModelPaths for GemmaModelPaths<PathBuf> {
    fn get_config_filename(&self) -> &PathBuf {
        &self.config_filename
    }
    fn get_tokenizer_filename(&self) -> &PathBuf {
        &self.tokenizer_filename
    }
    fn get_weight_filenames(&self) -> &[PathBuf] {
        &self.filenames
    }
    fn get_adapter_filenames(&self) -> &Option<Vec<(String, PathBuf)>> {
        &self.xlora_adapter_filenames
    }
    fn get_adapter_configs(&self) -> &Option<Vec<(String, LoraConfig)>> {
        &self.xlora_adapter_configs
    }
    fn get_classifier_config(&self) -> &Option<XLoraConfig> {
        &self.classifier_config
    }
    fn get_classifier_path(&self) -> &Option<PathBuf> {
        &self.classifier_path
    }
    fn get_ordering(&self) -> &Option<Ordering> {
        &self.xlora_ordering
    }
    fn get_template_filename(&self) -> &PathBuf {
        &self.template_filename
    }
}

pub struct GemmaPipeline {
    model: Model,
    tokenizer: Tokenizer,
    config: GemmaSpecificConfig,
    no_kv_cache: bool,
    chat_template: ChatTemplate,
}

pub struct GemmaLoader {
    model_id: String,
    config: GemmaSpecificConfig,
    quantized_model_id: Option<String>,
    quantized_filename: Option<String>,
    xlora_model_id: Option<String>,
    kind: ModelKind,
    xlora_order: Option<Ordering>,
    no_kv_cache: bool,
    chat_template: Option<String>,
    tokenizer_json: Option<String>,
}

#[derive(Clone, Copy)]
pub struct GemmaSpecificConfig {
    pub repeat_last_n: usize,
}

fn default_max_position_embeddings() -> usize {
    4096
}

#[derive(Deserialize)]
pub struct BasicConfig {
    pub attention_bias: bool,
    pub head_dim: usize,
    pub hidden_act: candle_nn::Activation,
    pub hidden_size: usize,
    pub intermediate_size: usize,
    pub num_attention_heads: usize,
    pub num_hidden_layers: usize,
    pub num_key_value_heads: usize,
    pub rms_norm_eps: f64,
    pub rope_theta: f64,
    pub vocab_size: usize,

    #[serde(default = "default_max_position_embeddings")]
    pub max_position_embeddings: usize,
}

#[derive(Error, Debug)]
enum TokenizerError {
    #[error("`{0}`")]
    Error(String),
}

impl GemmaLoader {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model_id: String,
        config: GemmaSpecificConfig,
        quantized_model_id: Option<String>,
        quantized_filename: Option<String>,
        xlora_model_id: Option<String>,
        kind: ModelKind,
        xlora_order: Option<Ordering>,
        no_kv_cache: bool,
        chat_template: Option<String>,
        tokenizer_json: Option<String>,
    ) -> Self {
        Self {
            model_id,
            config,
            quantized_model_id,
            quantized_filename,
            xlora_model_id,
            kind,
            xlora_order,
            no_kv_cache,
            chat_template,
            tokenizer_json,
        }
    }
}

impl Loader for GemmaLoader {
    fn download_model(
        &self,
        revision: Option<String>,
        token_source: TokenSource,
    ) -> Result<Box<dyn ModelPaths>> {
        let api = ApiBuilder::new()
            .with_progress(true)
            .with_token(Some(get_token(&token_source)?))
            .build()?;
        let revision = revision.unwrap_or("main".to_string());
        let api = api.repo(Repo::with_revision(
            self.model_id.clone(),
            RepoType::Model,
            revision.clone(),
        ));

        let tokenizer_filename = if let Some(ref p) = self.tokenizer_json {
            println!("Using tokenizer.json at `{p}`");
            PathBuf::from_str(p)?
        } else {
            api.get("tokenizer.json")?
        };

        let config_filename = api.get("config.json")?;

        let filenames = get_model_paths(
            revision.clone(),
            &token_source,
            &self.quantized_model_id,
            &self.quantized_filename,
            &api,
        )?;

        let XLoraPaths {
            adapter_configs,
            adapter_safetensors,
            classifier_path,
            xlora_order,
            xlora_config,
        } = get_xlora_paths(
            &self.xlora_model_id,
            &token_source,
            revision.clone(),
            &self.xlora_order,
        )?;

        let template_filename = api.get("tokenizer_config.json")?;

        Ok(Box::new(GemmaModelPaths {
            tokenizer_filename,
            config_filename,
            filenames,
            xlora_adapter_configs: adapter_configs,
            xlora_adapter_filenames: adapter_safetensors,
            classifier_path,
            classifier_config: xlora_config,
            xlora_ordering: xlora_order,
            template_filename,
        }))
    }

    fn _setup_model(
        &self,
        paths: &dyn ModelPaths,
        dtype: Option<DType>,
        device: &Device,
    ) -> Result<Box<Mutex<dyn Pipeline + Send + Sync>>> {
        let basic_config: BasicConfig =
            serde_json::from_slice(&std::fs::read(paths.get_config_filename())?)?;
        let config = Config {
            vocab_size: basic_config.vocab_size,
            hidden_size: basic_config.hidden_size,
            intermediate_size: basic_config.intermediate_size,
            num_hidden_layers: basic_config.num_hidden_layers,
            num_attention_heads: basic_config.num_attention_heads,
            num_key_value_heads: basic_config.num_key_value_heads,
            hidden_act: basic_config.hidden_act,
            max_position_embeddings: basic_config.max_position_embeddings,
            rms_norm_eps: basic_config.rms_norm_eps,
            rope_theta: basic_config.rope_theta,
            attention_bias: basic_config.attention_bias,
            head_dim: basic_config.head_dim,
        };
        let default_dtype = if device.is_cuda() {
            DType::BF16
        } else {
            DType::F32
        };

        println!("Loading model on {device:?}...");
        let model = match self.kind {
            ModelKind::QuantizedGGUF => unreachable!(),
            ModelKind::QuantizedGGML => unreachable!(),
            ModelKind::Normal => {
                let vb = from_mmaped_safetensors(
                    paths.get_weight_filenames().to_vec(),
                    Vec::new(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                let model = NormalModel::new(&config, vb)?;
                Model::Normal(model)
            }
            ModelKind::XLoraNormal => {
                let mut safetensors_paths = paths.get_weight_filenames().iter().collect::<Vec<_>>();
                safetensors_paths.push(paths.get_classifier_path().as_ref().unwrap());
                let vb = from_mmaped_safetensors(
                    safetensors_paths
                        .iter()
                        .map(|x| (*x).to_owned())
                        .collect::<Vec<_>>(),
                    paths
                        .get_adapter_filenames()
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|(_, x)| (*x).to_owned())
                        .collect::<Vec<_>>(),
                    dtype.unwrap_or(default_dtype),
                    device,
                    false,
                )?;

                let model = XLoraGemma::new(
                    &config,
                    vb,
                    paths.get_adapter_configs().as_ref().unwrap(),
                    paths.get_classifier_config().as_ref().unwrap().clone(),
                    paths.get_ordering().as_ref().unwrap().clone(),
                )?;
                Model::XLoraNormal(model)
            }
            ModelKind::XLoraGGUF => unreachable!(),
            ModelKind::XLoraGGML => unreachable!(),
        };
        println!("Model loaded.");

        let tokenizer = Tokenizer::from_file(paths.get_tokenizer_filename())
            .map_err(|e| TokenizerError::Error(e.to_string()))?;

        let chat_template: ChatTemplate = deserialize_chat_template!(paths, self);

        Ok(Box::new(Mutex::new(GemmaPipeline {
            model,
            tokenizer,
            config: self.config,
            no_kv_cache: self.no_kv_cache,
            chat_template,
        })))
    }
}

impl Pipeline for GemmaPipeline {
    fn forward(&mut self, input_toks: Box<[Rc<RefCell<Sequence>>]>, is_prompt: bool) -> Tensor {
        let (
            input_ids,
            input_ids_full,
            seqlen_offsets,
            seqlen_offsets_full,
            seqlen_offsets_kernel,
            seqlen_offsets_full_kernel,
        ) = calculate_inputs(
            input_toks,
            is_prompt,
            self.is_xlora(),
            self.device(),
            self.no_kv_cache,
        );
        let result = match self.model {
            Model::Normal(ref mut model) => {
                model.forward(&input_ids, &seqlen_offsets, &seqlen_offsets_kernel)
            }
            Model::XLoraNormal(ref mut model) => model.forward(
                &input_ids,
                input_ids_full.as_ref().unwrap(),
                &seqlen_offsets,
                seqlen_offsets_full.as_ref().unwrap(),
                &seqlen_offsets_kernel,
                seqlen_offsets_full_kernel.unwrap(),
                self.no_kv_cache,
            ),
        };
        match result {
            Ok(v) => v,
            Err(e) => {
                panic!("Model failed with error `{e}`. Please raise an issue.");
            }
        }
    }
    fn device(&self) -> &Device {
        match self.model {
            Model::Normal(ref model) => &model.device,
            Model::XLoraNormal(ref model) => &model.device,
        }
    }
    fn num_hidden_layers(&self) -> usize {
        self.cache().lock().len()
    }
    fn cache(&self) -> &Cache {
        match self.model {
            Model::Normal(ref model) => &model.cache,
            Model::XLoraNormal(ref model) => &model.cache,
        }
    }
    fn sample(&mut self, logits: Tensor, seq: Rc<RefCell<Sequence>>) -> Result<Logprobs> {
        let logits = logits
            .squeeze(0)
            .unwrap()
            .squeeze(0)
            .unwrap()
            .to_dtype(DType::F32)
            .unwrap();
        let start_at = deref_refcell!(seq)
            .get_toks()
            .len()
            .saturating_sub(self.config.repeat_last_n);
        let ctxt = deref_refcell!(seq).get_toks()[start_at..].to_vec();

        Ok(deref_mut_refcell!(seq)
            .logits_processor()
            .sample(&logits, Some(&ctxt))?)
    }
    fn tokenizer(&self) -> Tokenizer {
        self.tokenizer.clone()
    }
    fn eos_tok(&self) -> u32 {
        let eos_tok = match self.get_chat_template().eos_token {
            Either::Left(ref lit) => lit,
            Either::Right(ref added) => &added.content,
        };
        self.tokenizer
            .get_vocab(true)
            .get(eos_tok)
            .copied()
            .unwrap_or_else(|| panic!("Unable to extract `{eos_tok}` EOS token."))
    }
    fn name(&self) -> &'static str {
        "gemma"
    }
    fn get_max_seq_len(&self) -> usize {
        match &self.model {
            Model::Normal(model) => model.max_seq_len,
            Model::XLoraNormal(model) => model.max_seq_len,
        }
    }
    fn is_xlora(&self) -> bool {
        match &self.model {
            Model::Normal(_) => false,
            Model::XLoraNormal(_) => true,
        }
    }
    fn has_no_kv_cache(&self) -> bool {
        self.no_kv_cache
    }
    fn get_chat_template(&self) -> &ChatTemplate {
        &self.chat_template
    }
}
