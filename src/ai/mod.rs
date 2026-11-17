// Open Estate AI Analyst, OpenAI Responses API LLM Client, Grounded Q&A, Translation
// & Pure-Rust PDF Inspection / Preliminary Title / HOA / Disclosure Packet / Repair Addendum Review.

pub mod agent_harness;
pub mod analyst;
pub mod artifact_canvas;
pub mod ask;
pub mod brief;
pub mod doc_pipeline;
pub mod inspect;
pub mod llm;
pub mod pdf;
pub mod slides;
pub mod title_review;
pub mod translate;
// [feat/hoa-packet-ai]
pub mod hoa_review;
pub mod packet_review;
pub mod repair_addendum;
// [/feat/hoa-packet-ai]

pub use agent_harness::{
    build_agent_messages, build_agent_system_instructions,
    build_agent_system_instructions_with_config,
    build_agent_system_instructions_with_config_and_extra,
    build_agent_system_instructions_with_extra, build_deterministic_qa_response,
    build_deterministic_qa_response_for_role, build_housing_concept_tie_in,
    compact_housing_history, execute_housing_tool, housing_agent_step_schema,
    match_housing_concept_id, run_housing_agent_loop, run_housing_agent_loop_with_prompt,
    run_housing_agent_loop_with_prompt_and_role, CompactedHistory, ConceptTieIn,
    HousingSkillTool, UploadedDocContext, IMMUTABLE_HOUSING_AGENT_CONSTITUTION,
    MAX_COMPACTED_CHARS, MAX_TOOL_STEPS,
};
pub use analyst::{
    analysis_schema, build_deterministic_analyst_fallback, derive_neighborhood_scope,
    get_cached_neighborhood_narrative, put_neighborhood_narrative, run_ai_analyst,
    run_ai_analyst_with_prompt, run_ai_analyst_with_refresh, run_ai_analyst_with_store,
    sanitize_fair_housing, validate_analyst_data, NeighborhoodNarrativeScope, ANALYST_INSTRUCTIONS,
    NEIGHBORHOOD_NARRATIVE_TTL_SECS,
};
pub use artifact_canvas::{
    artifact_canvas_schema, build_deterministic_artifact, generate_llm_artifact_with_client,
    generate_llm_artifact_with_prompt, handle_report_artifact_post, scrub_artifact_value,
    DEFAULT_ARTIFACT_INSTRUCTIONS,
};
pub use ask::{
    ask_property_question, ask_property_question_for_role,
    ask_property_question_for_role_with_config, ask_property_question_with_config, ask_schema,
    normalize_assistant_role, role_system_prompt, AskPromptConfig, ASK_INSTRUCTIONS,
    DEFAULT_CHATBOX_MAX_WORDS, DEFAULT_CHATBOX_STYLE_PROMPT,
};
pub use brief::{build_brief, build_brief_with_limit, BRIEF_VERSION, DEFAULT_MAX_BRIEF_BYTES};
pub use inspect::{
    analyze_inspection, analyze_inspection_with_prompt, inspection_schema, INSPECTION_INSTRUCTIONS,
    SYSTEMS,
};
pub use llm::{
    compute_llm_cache_key, global_llm_cache, parse_json_loose, parse_model_spec,
    resolve_interactive_model_specs, resolve_llm_spec_for_call,
    resolve_llm_spec_for_call_with_active, resolve_match_model_specs, resolve_match_models,
    resolve_model_specs_with_effort, resolve_models, resolve_models_with_effort, responses_json,
    responses_json_with_cache, validate_json_schema, DbLlmCache, FnLlmCache, LayeredLlmCache,
    LlmCache, LlmCacheBackend, LlmCallOptions, LlmCallResult, LlmError, LlmInput, LlmMessage,
    LlmUsage, MemoryLlmCache, ModelSpec, ResolvedLlmCallSpec,
};
pub use pdf::{
    decode_base64_payload, extract_and_ocr_uploads_async, extract_document_pages,
    extract_document_pages_async, extract_embedded_images_from_pdf_page,
    extract_pdf_pages_from_bytes, normalize_and_validate_uploads, ocr_pdf_via_llm_responses,
    ExtractedDocument, ExtractedPdfImage, NormalizedUploadBatch, OcrConfig, PageExtract,
    PdfImageFormat, UploadedDocInput,
};
pub use slides::{
    build_deterministic_walkthrough_slides, ensure_walkthrough_slides_on_report,
    generate_llm_walkthrough_slides, generate_llm_walkthrough_slides_with_client,
    generate_llm_walkthrough_slides_with_prompt, generate_slides_for_report_json,
    handle_report_slides_post, scrub_walkthrough_deck_value, walkthrough_slides_schema,
    SLIDES_INSTRUCTIONS, WALKTHROUGH_SLIDE_IDS,
};
pub use title_review::{
    ai_title_schema, analyze_title_report, mask_title_pii, parse_title_report_text,
    redact_title_pii, PiiRedactionResult,
};
pub use translate::{
    extract_translatable_payload, resolve_lang, translate_report, translation_schema, LangConfig,
};
// [feat/hoa-packet-ai]
pub use hoa_review::{
    analyze_hoa_documents, classify_reserve_risk_tier, hoa_review_schema, parse_hoa_documents,
    DocPageCitation,
};
pub use packet_review::{
    analyze_disclosure_packet, analyze_packet_documents_deterministic,
    calibrate_regional_cost_multiplier, classify_packet_document, packet_review_schema,
    ContradictingCitation, SourceCitation, PACKET_DOC_CATEGORIES, TRIAGE_TIERS,
};
pub use pdf::{
    crc32_ieee, decode_base64_payload_with_limit, expand_uploaded_documents_with_zip,
    unpack_zip_archive_bytes, MAX_PACKET_DOCUMENTS,
};
pub use repair_addendum::{
    build_repair_addendum_deterministic, generate_repair_addendum, normalize_to_canonical_trade,
    repair_addendum_schema, AddendumLineItem, RepairAddendumInput, ADDENDUM_TRADES,
};
// [/feat/hoa-packet-ai]
