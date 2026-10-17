//! Round-trip and retrieval tests for the SQLite storage backend.
//!
//! The load-bearing ones are the `*_roundtrip_full` tests plus
//! [`props_keys_are_complete_per_type`]: together they guarantee every field of
//! every memory node survives a write/read cycle, so the grafeo → SQLite
//! migration cannot silently drop data.

use super::*;
use acowork_memory::quality::MemoryQualityConfig;
use acowork_memory::types::{EpisodicDecayConfig, MemoryQuery};
use acowork_memory::{KnowledgeSubType, MemoryProvider, PrivacyLevel, PromotionMetadata};
use chrono::TimeZone;
use std::collections::HashMap;

const DIM: usize = 4;

fn store() -> SqliteStore {
    SqliteStore::open_in_memory(DIM).expect("open in-memory store")
}

fn t(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

fn emb(value: f32) -> Vec<f32> {
    vec![value; DIM]
}

fn episode(content: &str) -> Episode {
    Episode {
        session_id: "s1".to_string(),
        turn_index: 0,
        role: "user".to_string(),
        content: content.to_string(),
        embedding: None,
        timestamp: t(1_700_000_000),
        consolidated: false,
        metadata: HashMap::new(),
        importance: 0.5,
        knowledge_subtype: None,
        normalized: None,
    }
}

fn knowledge(subject: &str, predicate: &str) -> KnowledgeNode {
    KnowledgeNode {
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: "Beijing".to_string(),
        sub_type: KnowledgeSubType::Fact,
        confidence: 0.9,
        source_episode_id: None,
        source_episode_ids: Vec::new(),
        promotion_metadata: None,
        embedding: None,
        status: NodeStatus::Active,
        created_at: t(1_700_000_000),
        updated_at: t(1_700_000_000),
        metadata: HashMap::new(),
        privacy: PrivacyLevel::Personal,
        importance: 0.5,
    }
}

fn procedural() -> ProceduralNode {
    ProceduralNode {
        id: None,
        name: "concise_output".to_string(),
        trigger_condition: "user asks for summary".to_string(),
        action_pattern: "reply in three sentences".to_string(),
        success_count: 0,
        fail_count: 0,
        confidence: 0.5,
        activation_count: 0,
        source_skill: None,
        learned_from: "unknown".to_string(),
        embedding: Vec::new(),
        status: NodeStatus::Active,
        created_at: t(1_700_000_000),
        updated_at: t(1_700_000_000),
        source_episode_ids: Vec::new(),
        promotion_metadata: None,
        metadata: HashMap::new(),
    }
}

fn autobiographical() -> AutobiographicalNode {
    AutobiographicalNode {
        id: None,
        category: AutobioCategory::Identity,
        key: "language".to_string(),
        value: "zh-CN".to_string(),
        confidence: 0.9,
        source_episode_id: None,
        embedding: None,
        status: NodeStatus::Active,
        created_at: t(1_700_000_000),
        updated_at: t(1_700_000_000),
        source_episode_ids: Vec::new(),
        promotion_metadata: None,
        source: "user_statement".to_string(),
        metadata: HashMap::new(),
    }
}

fn promotion_metadata() -> PromotionMetadata {
    PromotionMetadata {
        promoted_at: t(1_700_000_500),
        promoted_by: "episodic_distiller".to_string(),
        evidence_episode_ids: vec![10, 11, 12],
        evidence_span_days: 3,
        llm_judge_confidence: 0.8,
        llm_judge_reasoning: "repeated across three episodes".to_string(),
    }
}

/// Compare two nodes by their full JSON, ignoring the store-assigned `id`.
fn assert_same_node<T: serde::Serialize>(original: &T, restored: &T) {
    let mut a = serde_json::to_value(original).unwrap();
    let mut b = serde_json::to_value(restored).unwrap();
    for value in [&mut a, &mut b] {
        if let Some(map) = value.as_object_mut() {
            map.insert("id".to_string(), serde_json::Value::Null);
        }
    }
    assert_eq!(a, b);
}

fn props_keys(store: &SqliteStore, id: u64, label: &str) -> Vec<String> {
    let props = store.load_props(id, label).unwrap().expect("row exists");
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&props).unwrap();
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

// ── Round-trips ──────────────────────────────────────────────────────────

#[test]
fn episode_roundtrip_full() {
    let store = store();
    let mut original = episode("Hello, 世界");
    original.turn_index = 7;
    original.role = "assistant".to_string();
    original.embedding = Some(emb(0.25));
    original.consolidated = true;
    original.importance = 0.75;
    original.knowledge_subtype = Some(KnowledgeSubType::Preference);
    original.metadata.insert(
        "topic".to_string(),
        serde_json::Value::String("greeting".to_string()),
    );
    original.metadata.insert(
        "sentiment".to_string(),
        serde_json::json!({ "score": 0.9, "label": "positive" }),
    );

    let id = store.store_episode(&original).unwrap();
    let restored = store.get_episode(id).unwrap().unwrap();

    assert_same_node(&original, &restored);
    assert_eq!(restored.timestamp, original.timestamp);
}

#[test]
fn episode_roundtrip_minimal() {
    let store = store();
    let original = episode("no embedding, no metadata");

    let id = store.store_episode(&original).unwrap();
    let restored = store.get_episode(id).unwrap().unwrap();

    assert_same_node(&original, &restored);
    assert!(restored.embedding.is_none());
    assert_eq!(store.count_nodes_with_embedding().unwrap(), 0);
}

#[test]
fn knowledge_roundtrip_full() {
    let store = store();
    let mut original = knowledge("user", "prefers");
    original.object = "concise output".to_string();
    original.sub_type = KnowledgeSubType::Preference;
    original.confidence = 0.85;
    original.source_episode_id = Some(42);
    original.source_episode_ids = vec![42, 43];
    original.promotion_metadata = Some(promotion_metadata());
    original.embedding = Some(emb(0.5));
    original.status = NodeStatus::Dormant;
    original.privacy = PrivacyLevel::Sensitive;
    original.importance = 0.9;
    original
        .metadata
        .insert("source".to_string(), serde_json::json!("manifest"));

    let id = store.store_knowledge(&original).unwrap();
    let restored = store.get_knowledge(id).unwrap().unwrap();

    assert_same_node(&original, &restored);
}

#[test]
fn procedural_roundtrip_full() {
    let store = store();
    let mut original = procedural();
    original.success_count = 5;
    original.fail_count = 2;
    original.confidence = 0.8;
    original.activation_count = 9;
    original.source_skill = Some("summarize".to_string());
    original.learned_from = "user_feedback".to_string();
    original.embedding = emb(0.1);
    original.status = NodeStatus::Dormant;
    original.source_episode_ids = vec![1, 2];
    original.promotion_metadata = Some(promotion_metadata());

    let id = store.store_procedural(&original).unwrap();
    let restored = store.get_procedural(id).unwrap().unwrap();

    assert_same_node(&original, &restored);
}

#[test]
fn autobiographical_roundtrip_full() {
    let store = store();
    let mut original = autobiographical();
    original.category = AutobioCategory::Preference;
    original.confidence = 0.7;
    original.source_episode_id = Some(7);
    original.source_episode_ids = vec![7];
    original.promotion_metadata = Some(promotion_metadata());
    original.embedding = Some(emb(0.3));
    original.source = "important_event".to_string();

    let id = store.store_autobiographical(&original).unwrap();
    let restored = store.get_autobiographical(id).unwrap().unwrap();

    assert_same_node(&original, &restored);
}

#[test]
fn timestamps_preserve_micros() {
    let store = store();
    let mut original = episode("micros");
    original.timestamp = Utc.timestamp_micros(1_700_000_000_123_456).unwrap();

    let id = store.store_episode(&original).unwrap();
    let restored = store.get_episode(id).unwrap().unwrap();

    assert_eq!(
        restored.timestamp.timestamp_micros(),
        original.timestamp.timestamp_micros()
    );
}

// ── Field completeness ───────────────────────────────────────────────────

#[test]
fn props_keys_are_complete_per_type() {
    let store = store();

    // `id` and `embedding` are the only projected-out fields; everything else
    // must be present or the migration would silently drop it.
    let ep_id = store.store_episode(&episode("x")).unwrap();
    assert_eq!(
        props_keys(&store, ep_id, labels::EPISODIC),
        [
            "consolidated",
            "content",
            "importance",
            "knowledge_subtype",
            "metadata",
            "normalized",
            "role",
            "session_id",
            "timestamp",
            "turn_index",
        ]
    );

    let kn_id = store.store_knowledge(&knowledge("user", "likes")).unwrap();
    assert_eq!(
        props_keys(&store, kn_id, labels::KNOWLEDGE),
        [
            "confidence",
            "created_at",
            "importance",
            "metadata",
            "object",
            "predicate",
            "privacy",
            "promotion_metadata",
            "source_episode_id",
            "source_episode_ids",
            "status",
            "sub_type",
            "subject",
            "updated_at",
        ]
    );

    let pr_id = store.store_procedural(&procedural()).unwrap();
    assert_eq!(
        props_keys(&store, pr_id, labels::PROCEDURAL),
        [
            "action_pattern",
            "activation_count",
            "confidence",
            "created_at",
            "fail_count",
            "learned_from",
            "metadata",
            "name",
            "promotion_metadata",
            "source_episode_ids",
            "source_skill",
            "status",
            "success_count",
            "trigger_condition",
            "updated_at",
        ]
    );

    let au_id = store.store_autobiographical(&autobiographical()).unwrap();
    assert_eq!(
        props_keys(&store, au_id, labels::AUTOBIOGRAPHICAL),
        [
            "category",
            "confidence",
            "created_at",
            "key",
            "metadata",
            "promotion_metadata",
            "source",
            "source_episode_id",
            "source_episode_ids",
            "status",
            "updated_at",
            "value",
        ]
    );
}

// ── Persistence / lifecycle ──────────────────────────────────────────────

#[test]
fn persistence_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite");

    let id = {
        let store = SqliteStore::open(&path, DIM).unwrap();
        let mut node = knowledge("user", "lives_in");
        node.embedding = Some(emb(0.5));
        store.store_knowledge(&node).unwrap()
    };

    let store = SqliteStore::open(&path, DIM).unwrap();
    let restored = store.get_knowledge(id).unwrap().unwrap();
    assert_eq!(restored.object, "Beijing");
    assert_eq!(restored.embedding.as_ref().map(Vec::len), Some(DIM));
}

#[test]
fn delete_node_removes_vector_and_fts() {
    let store = store();
    let mut ep = episode("deletable rust content");
    ep.embedding = Some(emb(1.0));
    let id = store.store_episode(&ep).unwrap();

    assert!(store.delete_node(id).unwrap());
    assert!(!store.delete_node(id).unwrap());
    assert_eq!(store.node_count().unwrap(), 0);
    assert_eq!(store.count_nodes_with_embedding().unwrap(), 0);
    assert!(
        store
            .text_search(labels::EPISODIC, "deletable", 10)
            .unwrap()
            .is_empty()
    );
}

// ── Episodic queries ─────────────────────────────────────────────────────

#[test]
fn episodes_by_session_and_recent_first() {
    let store = store();
    for (i, session) in ["a", "b", "a"].iter().enumerate() {
        let mut ep = episode(&format!("m{i}"));
        ep.session_id = (*session).to_string();
        ep.timestamp = t(1_700_000_000 + i as i64);
        store.store_episode(&ep).unwrap();
    }

    let sessions = store.search_episodes_by_session("a", 10).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions[0].content, "m2"); // newest first

    let all = store.list_all_episodes(10).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn mark_and_cleanup_consolidated() {
    let store = store();
    // `count_unconsolidated_episodes` is the distiller's backlog gauge, so it
    // only counts subtype-classified episodes; unclassified fragments stay in
    // the episodic layer forever and must not inflate it.
    let mut old_ep = episode("old");
    old_ep.knowledge_subtype = Some(KnowledgeSubType::Fact);
    let mut recent_ep = episode("recent");
    recent_ep.knowledge_subtype = Some(KnowledgeSubType::Fact);
    let old = store.store_episode(&old_ep).unwrap();
    let recent = store.store_episode(&recent_ep).unwrap();
    store.mark_episode_consolidated(old).unwrap();
    assert_eq!(store.count_unconsolidated_episodes().unwrap(), 1);

    // Backdate the consolidated episode past the retention window.
    {
        let conn = store.lock();
        conn.execute(
            "UPDATE nodes SET created_at = ?1 WHERE id = ?2",
            params![ts_text(t(1_600_000_000)), old as i64],
        )
        .unwrap();
    }

    assert_eq!(store.cleanup_old_episodes(30).unwrap(), 1);
    assert!(store.get_episode(old).unwrap().is_none());
    assert!(store.get_episode(recent).unwrap().is_some());
    let unconsolidated = store.get_unconsolidated_episodes(10).unwrap();
    assert_eq!(unconsolidated.len(), 1);
    assert_eq!(unconsolidated[0].content, "recent");
}

// ── Text search ──────────────────────────────────────────────────────────

#[test]
fn text_search_matches_chinese_substring() {
    let store = store();
    store
        .store_episode(&episode("用户喜欢简洁的输出风格"))
        .unwrap();
    store.store_episode(&episode("今天天气不错")).unwrap();

    let hits = store.search_episodes_by_keyword("简洁的输出", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].0.content.contains("简洁"));
}

#[test]
fn text_search_ors_whitespace_separated_terms() {
    let store = store();
    store
        .store_episode(&episode(
            "the gateway binds 19876 while the broker takes 19875",
        ))
        .unwrap();

    // The two words are far apart in the text, so matching the query as one
    // phrase would find nothing — the union of terms is what makes a plain
    // multi-word ask work.
    let hits = store
        .search_episodes_by_keyword("gateway broker", 10)
        .unwrap();
    assert_eq!(hits.len(), 1, "terms must be OR'd, not required adjacent");
}

#[test]
fn text_search_short_query_falls_back_to_like() {
    let store = store();
    store.store_episode(&episode("用户喜欢简洁的输出")).unwrap();

    // Two characters — below the trigram minimum, so the LIKE path must fire.
    let hits = store.search_episodes_by_keyword("简洁", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, 0.0);
}

#[test]
fn text_search_is_label_isolated() {
    let store = store();
    store
        .store_episode(&episode("shared keyword alpha"))
        .unwrap();
    let mut node = knowledge("user", "alpha");
    node.object = "shared keyword alpha".to_string();
    store.store_knowledge(&node).unwrap();

    assert_eq!(
        store
            .text_search(labels::EPISODIC, "keyword", 10)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .text_search(labels::KNOWLEDGE, "keyword", 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn text_search_like_escapes_wildcards() {
    let store = store();
    store.store_episode(&episode("100% done")).unwrap();
    store.store_episode(&episode("unrelated")).unwrap();

    // A bare '%' would match everything without escaping.
    let hits = store.search_episodes_by_keyword("0%", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].0.content.contains('%'));
}

// ── Vector search ────────────────────────────────────────────────────────

#[test]
fn vector_search_ranks_by_cosine() {
    let store = store();
    let mut near = episode("near");
    near.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    store.store_episode(&near).unwrap();

    let mut far = episode("far");
    far.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    store.store_episode(&far).unwrap();

    let hits = store
        .search_episodes_by_embedding(&[1.0, 0.0, 0.0, 0.0], 10)
        .unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].0.content, "near");
    assert!((hits[0].1 - 1.0).abs() < 1e-6);
    assert!(hits[1].1.abs() < 1e-6);
}

#[test]
fn vector_search_skips_dim_mismatch() {
    let store = store();
    let mut ok = episode("right dim");
    ok.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    store.store_episode(&ok).unwrap();

    let mut other = episode("wrong dim");
    other.embedding = Some(vec![1.0, 0.0, 0.0]);
    store.store_episode(&other).unwrap();

    let hits = store
        .search_episodes_by_embedding(&[1.0, 0.0, 0.0, 0.0], 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.content, "right dim");
}

// ── Semantic helpers ─────────────────────────────────────────────────────

#[test]
fn store_knowledge_dedups_same_subject_predicate() {
    let store = store();
    let mut first = knowledge("user", "lives_in");
    first.object = "Beijing".to_string();
    let id = store.store_knowledge(&first).unwrap();

    let mut second = knowledge("user", "lives_in");
    second.object = "Shanghai".to_string();
    let second_id = store.store_knowledge(&second).unwrap();

    assert_eq!(id, second_id);
    assert_eq!(store.node_count_by_label(labels::KNOWLEDGE).unwrap(), 1);
    let stored = store.get_knowledge(id).unwrap().unwrap();
    assert_eq!(stored.object, "Shanghai");
}

#[test]
fn find_procedural_by_trigger_is_case_insensitive() {
    let store = store();
    let mut node = procedural();
    node.trigger_condition = "User asks for a Summary".to_string();
    store.store_procedural(&node).unwrap();

    let hits = store.find_procedural_by_trigger("summary", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "concise_output");
}

#[test]
fn find_autobiographical_by_key_and_category() {
    let store = store();
    store.store_autobiographical(&autobiographical()).unwrap();

    let by_key = store.find_autobiographical_by_key("language").unwrap();
    assert_eq!(by_key.unwrap().value, "zh-CN");

    let by_category = store
        .find_autobiographical_by_category(AutobioCategory::Identity)
        .unwrap();
    assert_eq!(by_category.len(), 1);
    assert!(
        store
            .find_autobiographical_by_category(AutobioCategory::History)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn store_autobiographical_forces_active() {
    let store = store();
    let mut node = autobiographical();
    node.status = NodeStatus::Dormant;

    let id = store.store_autobiographical(&node).unwrap();
    let restored = store.get_autobiographical(id).unwrap().unwrap();
    assert_eq!(restored.status, NodeStatus::Active);
}

// ── Hybrid retrieval fusion (ADR-082 D5) ─────────────────────────────────

/// ADR-082 §1.4 regression: a BM25-only hit must survive the cosine floor.
///
/// The engine-era fusion applied `min_score` in the *fused* score domain, which
/// on the vector-only path meant `cos >= 1` — every Chinese `memory_recall`
/// query returned nothing. Here the floor gates the vector source only, so the
/// lexical hit is kept even though its embedding is orthogonal to the query.
#[test]
fn hybrid_keeps_lexical_hit_whose_embedding_is_far() {
    let store = store();

    let mut lexical_only = episode("北京今天下雨吗");
    lexical_only.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    let lexical_id = store.store_episode(&lexical_only).unwrap();

    let mut vector_only = episode("completely unrelated wording");
    vector_only.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let vector_id = store.store_episode(&vector_only).unwrap();

    let hits = store
        .hybrid_search_full(
            labels::EPISODIC,
            "北京今天下雨吗",
            &[1.0, 0.0, 0.0, 0.0],
            10,
            1.0,
            1.0,
            Some(0.3),
        )
        .unwrap();

    let ids: Vec<u64> = hits.iter().map(|(id, _)| *id).collect();
    assert!(
        ids.contains(&lexical_id),
        "lexical hit dropped by the cosine floor: {ids:?}"
    );
    assert!(ids.contains(&vector_id));

    // Both are single-source rank-1 hits, so the id tie-break decides; what
    // matters is that neither was filtered out.
    assert_eq!(hits.len(), 2);
    // Scores are the RRF fused value: a single-source rank-1 hit gets
    // `weight / 61`. Both sources are gated or absent (the lexical hit has
    // a `cos = 0` embedding that fails the 0.3 floor, the vector hit has no
    // lexical overlap), so each surviving hit is single-source and both
    // land on the same `1 / 61` regardless of which source they came from.
    let expected = 1.0 / 61.0;
    for (id, score) in &hits {
        assert!(
            (score - expected).abs() < 1e-9,
            "id {id} got score {score}, expected single-source RRF rank-1 (1/61)"
        );
    }
}

/// ADR-095 D2: analyzed terms drive FTS **as terms**, not as re-split text.
///
/// The regression this guards is subtle: if the engine joined terms back into
/// a string and re-split it, a multi-word term (or a CJK bigram) would be
/// tokenized twice and the analyzer's work would be silently undone. Here the
/// term list is deliberately one that whitespace splitting could not produce.
#[test]
fn hybrid_uses_analyzed_terms_instead_of_resplitting_raw() {
    let store = store();

    let mut a = episode("rust borrow checker lifetime rules");
    a.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let id_a = store.store_episode(&a).unwrap();

    let mut b = episode("tokio scheduler task budget");
    b.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    let id_b = store.store_episode(&b).unwrap();

    // Raw text mentions neither word in a splittable form; the analyzer hands
    // over exact terms that only match `b`.
    let terms = vec!["tokio".to_string(), "scheduler".to_string()];
    let hits = store
        .hybrid_search_detailed(
            labels::EPISODIC,
            &acowork_core::retrieval::LexicalQuery { raw: "zzz-unanalyzable", terms: &terms },
            Some(&[1.0, 0.0, 0.0, 0.0]),
            10,
            &acowork_core::retrieval::FusionPolicy::SEMANTIC_4TO1,
        )
        .unwrap();

    let ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
    assert!(ids.contains(&id_b), "analyzed terms did not reach FTS: {ids:?}");

    // SEMANTIC_4TO1 has no cosine floor, so the vector source ranks *every*
    // node and `matched_by` alone cannot show which source did the work.
    // `text_rank` is the honest discriminator: it exists only if FTS returned
    // the id, and FTS ran on the analyzer's terms and nothing else.
    let by_id = |id| hits.iter().find(|h| h.id == id).unwrap();
    assert!(
        by_id(id_b).text_rank.is_some(),
        "analyzed terms did not reach FTS"
    );
    assert!(
        by_id(id_a).text_rank.is_none(),
        "raw fallback leaked into the lexical source"
    );
}

/// The fallback half of D2: no terms ⇒ the raw text drives FTS.
#[test]
fn hybrid_falls_back_to_raw_when_analyzer_produced_nothing() {
    let store = store();

    let mut a = episode("kafka consumer group rebalance");
    a.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    let id_a = store.store_episode(&a).unwrap();

    // `raw_only` is what every pre-ADR-095 caller now maps to.
    let hits = store
        .hybrid_search_detailed(
            labels::EPISODIC,
            &acowork_core::retrieval::LexicalQuery::raw_only("kafka rebalance"),
            Some(&[1.0, 0.0, 0.0, 0.0]),
            10,
            &acowork_core::retrieval::FusionPolicy::SEMANTIC_4TO1,
        )
        .unwrap();
    let ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
    assert!(ids.contains(&id_a), "raw fallback lost recall: {ids:?}");
    // The raw text matched, which is the entire point of the fallback. The hit
    // also carries a vector rank because the memory policy gates nothing —
    // asserting `matched_by == Text` here would encode the wrong floor.
    assert!(
        hits.iter().find(|h| h.id == id_a).unwrap().text_rank.is_some(),
        "raw fallback produced no lexical evidence"
    );
}

/// A hit found by *both* sources must outrank a hit found by only one.
#[test]
fn hybrid_ranks_dual_source_hit_first() {
    let store = store();

    let mut both = episode("project codename is blue whale");
    both.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let both_id = store.store_episode(&both).unwrap();

    let mut vector_only = episode("nothing lexically shared here");
    vector_only.embedding = Some(vec![0.9, 0.1, 0.0, 0.0]);
    let vector_id = store.store_episode(&vector_only).unwrap();

    let hits = store
        .hybrid_search_full(
            labels::EPISODIC,
            "project codename is blue whale",
            &[1.0, 0.0, 0.0, 0.0],
            10,
            1.0,
            1.0,
            Some(0.3),
        )
        .unwrap();

    assert_eq!(
        hits[0].0, both_id,
        "dual-source hit lost to a vector-only hit"
    );
    assert!(hits.iter().any(|(id, _)| *id == vector_id));
}

/// The vector source still honours the absolute cosine floor; the floor is not
/// applied to the text source.
#[test]
fn hybrid_gates_vector_source_by_min_cosine() {
    let store = store();

    let mut far = episode("qxz wording that the query cannot lexically match");
    far.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    let far_id = store.store_episode(&far).unwrap();

    let query = &[1.0, 0.0, 0.0, 0.0];

    let gated = store
        .hybrid_search_full(labels::EPISODIC, "zzz", query, 10, 1.0, 1.0, Some(0.3))
        .unwrap();
    assert!(gated.is_empty(), "cos 0 survived a 0.3 floor: {gated:?}");

    let ungated = store
        .hybrid_search_full(labels::EPISODIC, "zzz", query, 10, 1.0, 1.0, Some(-1.0))
        .unwrap();
    assert_eq!(ungated.len(), 1);
    assert_eq!(ungated[0].0, far_id);
    // Score is the RRF fused value for a single-source rank-1 hit:
    // `text_weight / (60 + 0 + 1) = 1 / 61 ≈ 0.01639` (text only; the vector
    // source was disabled because cos = 0 < the gate in the prior test).
    assert!(
        (ungated[0].1 - 1.0 / 61.0).abs() < 1e-9,
        "got {:?}, expected text-only RRF rank-1 (1/61)",
        ungated[0].1
    );
}

/// Even with an impossibly high floor, a lexical hit is retained: a BM25 match
/// is independent evidence, and a weak/absent embedding must not kill it.
#[test]
fn hybrid_text_source_ignores_min_cosine() {
    let store = store();

    let mut node = episode("独一无二的中文词组");
    node.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    let id = store.store_episode(&node).unwrap();

    let hits = store
        .hybrid_search_full(
            labels::EPISODIC,
            "独一无二的中文词组",
            &[1.0, 0.0, 0.0, 0.0],
            10,
            1.0,
            1.0,
            Some(0.99),
        )
        .unwrap();

    assert_eq!(hits.len(), 1, "lexical hit gated away: {hits:?}");
    assert_eq!(hits[0].0, id);
}

/// Fusion must not leak across labels.
#[test]
fn hybrid_search_is_label_isolated() {
    let store = store();
    let mut lexical = episode("shared keyword alpha");
    lexical.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    store.store_episode(&lexical).unwrap();

    let hits = store
        .hybrid_search_full(
            labels::KNOWLEDGE,
            "shared keyword alpha",
            &[1.0, 0.0, 0.0, 0.0],
            10,
            1.0,
            1.0,
            Some(0.3),
        )
        .unwrap();
    assert!(hits.is_empty(), "{hits:?}");
}

/// `k` caps the fused output, and an empty query still allows vector-only hits.
#[test]
fn hybrid_respects_limit_and_handles_empty_query() {
    let store = store();
    for i in 0..5 {
        let mut node = episode(&format!("filler number {i}"));
        node.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
        store.store_episode(&node).unwrap();
    }

    let hits = store
        .hybrid_search_full(
            labels::EPISODIC,
            "filler",
            &[1.0, 0.0, 0.0, 0.0],
            2,
            1.0,
            1.0,
            None,
        )
        .unwrap();
    assert_eq!(hits.len(), 2);

    let vector_only = store
        .hybrid_search_full(
            labels::EPISODIC,
            "   ",
            &[1.0, 0.0, 0.0, 0.0],
            10,
            1.0,
            1.0,
            None,
        )
        .unwrap();
    assert_eq!(vector_only.len(), 5);
}

/// `hybrid_search_filtered` is the equal-weight, ungated entry point.
#[test]
fn hybrid_search_filtered_delegates() {
    let store = store();
    let mut node = episode("delegation works");
    node.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let id = store.store_episode(&node).unwrap();

    let hits = store
        .hybrid_search_filtered(
            labels::EPISODIC,
            "content",
            "embedding",
            "delegation works",
            &[1.0, 0.0, 0.0, 0.0],
            10,
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, id);
}

/// Equal-weight RRF makes single-source rank-1 hits tie exactly, so the result
/// order must not depend on `HashMap` iteration order (randomized per process).
#[test]
fn hybrid_order_is_deterministic_across_calls() {
    let store = store();
    for i in 0..5 {
        let mut node = episode(&format!("filler number {i}"));
        node.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
        store.store_episode(&node).unwrap();
    }

    let expected = store
        .hybrid_search_full(
            labels::EPISODIC,
            "filler",
            &[1.0, 0.0, 0.0, 0.0],
            5,
            1.0,
            1.0,
            None,
        )
        .unwrap();
    assert_eq!(expected.len(), 5);
    assert_eq!(
        expected.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "ties must fall back to node id"
    );

    for _ in 0..20 {
        let again = store
            .hybrid_search_full(
                labels::EPISODIC,
                "filler",
                &[1.0, 0.0, 0.0, 0.0],
                5,
                1.0,
                1.0,
                None,
            )
            .unwrap();
        assert_eq!(expected, again);
    }
}

// ── MemoryProvider integration (ADR-082 D1) ──────────────────────────────
//
// These exercise the trait surface the Runtime actually holds, so they are the
// tests that prove the four feature chains (memory_store / memory_recall /
// distillation / forgetting) survive the backend swap.

/// Content rendering must match the grafeo provider byte-for-byte, because
/// prompts and the `memory_recall` tool consume it directly.
#[test]
fn provider_renders_content_per_node_type() {
    let store = store();

    let episode_id = store.store_episode(&episode("用户问了天气")).unwrap();
    let knowledge_id = store
        .store_knowledge(&knowledge("user", "lives in"))
        .unwrap();

    let mut proc = procedural();
    proc.trigger_condition = "用户要求详细解释".to_string();
    proc.action_pattern = "先给结论".to_string();
    let proc_id = store.store_procedural(&proc).unwrap();

    let mut auto = autobiographical();
    auto.category = AutobioCategory::Identity;
    auto.key = "name".to_string();
    auto.value = "大鱼".to_string();
    let auto_id = store.store_autobiographical(&auto).unwrap();

    assert_eq!(
        store.get_node_content(episode_id).unwrap().as_deref(),
        Some("用户问了天气")
    );
    assert_eq!(
        store.get_node_content(knowledge_id).unwrap().as_deref(),
        Some("user lives in Beijing")
    );
    assert_eq!(
        store.get_node_content(proc_id).unwrap().as_deref(),
        Some("当 用户要求详细解释 时，优先 先给结论")
    );
    assert_eq!(
        store.get_node_content(auto_id).unwrap().as_deref(),
        Some("Identity: name: 大鱼")
    );
    assert_eq!(store.get_node_content(9_999).unwrap(), None);
}

/// `MemoryManager` filters the current session out of retrieval results through
/// this accessor; it must answer for episodic nodes and stay `None` elsewhere.
#[test]
fn provider_exposes_session_id_and_status() {
    let store = store();
    let mut ep = episode("session scoped");
    ep.session_id = "sess-7".to_string();
    let ep_id = store.store_episode(&ep).unwrap();
    let kn_id = store.store_knowledge(&knowledge("user", "likes")).unwrap();

    assert_eq!(
        store.get_node_session_id(ep_id).unwrap().as_deref(),
        Some("sess-7")
    );
    assert_eq!(store.get_node_session_id(kn_id).unwrap(), None);
    assert_eq!(
        store.get_node_status(ep_id).unwrap(),
        Some(NodeStatus::Active)
    );
    assert_eq!(store.get_node_status(9_999).unwrap(), None);
}

/// `memory_recall --since/--until` filters on `created_at`. Episodes carry
/// their timestamp in that column instead of a `created_at` property, so the
/// accessor must answer for them too (the grafeo path returned `None`, which
/// silently disabled the filter).
#[test]
fn provider_created_at_covers_episodes() {
    let store = store();
    let mut old = episode("old news");
    old.timestamp = t(1_600_000_000);
    let old_id = store.store_episode(&old).unwrap();

    assert_eq!(
        store.get_node_created_at(old_id).unwrap(),
        Some(t(1_600_000_000))
    );

    let query = MemoryQuery {
        filters: acowork_memory::types::MemoryFilters {
            time_range: Some((t(1_700_000_000), t(1_800_000_000))),
            ..Default::default()
        },
        ..MemoryQuery::new("news")
    };
    // The provider itself does not filter; the accessor is what makes the
    // manager-side filter possible.
    let created = store.get_node_created_at(old_id).unwrap().unwrap();
    let (since, until) = query.filters.time_range.unwrap();
    assert!(created < since && created <= until);
}

/// The `status` column is authoritative: forgetting flips it without touching
/// `props`, so reading `props.status` would resurrect decayed episodes.
#[test]
fn provider_status_reads_column_not_props() {
    let store = store();
    let mut ep = episode("will decay");
    ep.timestamp = t(1_700_000_000);
    let id = store.store_episode(&ep).unwrap();

    store.transition_to_dormant(id).unwrap();
    assert_eq!(
        store.get_node_status(id).unwrap(),
        Some(NodeStatus::Dormant)
    );
}

/// `get_episodes_by_subtype` honours the subtype filter and orders newest
/// first, and the backlog count uses the same predicate as the scan.
#[test]
fn provider_episodes_by_subtype_filters_and_orders() {
    let store = store();
    let mut older = episode("older fact");
    older.timestamp = t(1_600_000_000);
    older.knowledge_subtype = Some(KnowledgeSubType::Fact);
    let older_id = store.store_episode(&older).unwrap();

    let mut newer = episode("newer preference");
    newer.timestamp = t(1_700_000_000);
    newer.knowledge_subtype = Some(KnowledgeSubType::Preference);
    let newer_id = store.store_episode(&newer).unwrap();

    let facts = store
        .get_episodes_by_subtype(Some(KnowledgeSubType::Fact), 10)
        .unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].0, older_id);

    let all = store.get_episodes_by_subtype(None, 10).unwrap();
    assert_eq!(
        all.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![newer_id, older_id],
        "newest first: an episode that keeps failing must not hold the queue"
    );
    // Both carry a subtype, so the backlog count sees both (same predicate).
    assert_eq!(store.count_unconsolidated_episodes().unwrap(), 2);
}

/// Forgetting: Active → Dormant once retention drops below the threshold, but
/// only when the config is enabled.
#[test]
fn provider_decay_scan_dormants_old_episodes() {
    let store = store();
    let mut ancient = episode("ancient");
    // ~3.3 half-lives of age with the default 180-day half-life → retention
    // well under the 0.1 dormant threshold.
    ancient.timestamp = Utc::now() - TimeDelta::days(1200);
    let ancient_id = store.store_episode(&ancient).unwrap();

    // Note: the shared `episode()` fixture is pinned at 2023-11-14, which is
    // already decades past the threshold — "fresh" must be explicit.
    let mut fresh = episode("fresh");
    fresh.timestamp = Utc::now();
    let fresh_id = store.store_episode(&fresh).unwrap();

    let disabled = EpisodicDecayConfig::default();
    assert!(!disabled.enabled);
    let noop = store.run_episodic_decay_scan(&disabled).unwrap();
    assert_eq!(noop.to_dormant, 0);
    assert_eq!(
        store.get_node_status(ancient_id).unwrap(),
        Some(NodeStatus::Active)
    );

    let enabled = EpisodicDecayConfig {
        enabled: true,
        ..Default::default()
    };
    let result = store.run_episodic_decay_scan(&enabled).unwrap();
    assert_eq!(result.to_dormant, 1);
    assert_eq!(result.purged, 0);
    assert_eq!(
        store.get_node_status(ancient_id).unwrap(),
        Some(NodeStatus::Dormant)
    );
    assert_eq!(
        store.get_node_status(fresh_id).unwrap(),
        Some(NodeStatus::Active)
    );
}

/// The archive step must copy the node into `purge_log` before deleting it —
/// forgetting is recoverable, never destructive.
#[test]
fn provider_decay_archives_before_deleting() {
    let store = store();
    let mut ancient = episode("archived memory");
    ancient.timestamp = Utc::now() - TimeDelta::days(1200);
    let id = store.store_episode(&ancient).unwrap();

    let enabled = EpisodicDecayConfig {
        enabled: true,
        ..Default::default()
    };
    store.run_episodic_decay_scan(&enabled).unwrap();

    // Age the dormancy past the 90-day archive deadline.
    {
        let conn = store.lock();
        conn.execute(
            "UPDATE nodes SET props = json_set(props, '$.dormant_since', json_quote(?2)) WHERE id = ?1",
            params![id as i64, ts_text(Utc::now() - TimeDelta::days(120))],
        )
        .unwrap();
    }

    let result = store.run_episodic_decay_scan(&enabled).unwrap();
    assert_eq!(result.purged, 1);
    assert!(store.get_episode(id).unwrap().is_none());

    let conn = store.lock();
    let (content, reason): (String, String) = conn
        .query_row(
            "SELECT content, reason FROM purge_log WHERE node_id = ?1",
            params![id as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(content, "archived memory");
    assert!(reason.contains("dormant_days=120"), "{reason}");
}

/// `apply_quality_config` must actually reach the write path, not just be
/// recorded: raising the dedup threshold stops the (subject, predicate) merge.
///
/// The two stores are deliberate — a merge rewrites the stored embedding, so
/// probing both thresholds on one store would compare against a moved target.
#[test]
fn provider_quality_config_drives_dedup() {
    let similar = |store: &SqliteStore| {
        let mut first = knowledge("user", "lives in");
        first.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
        let first_id = store.store_knowledge(&first).unwrap();

        // cos 0.96 against the first embedding.
        let mut second = knowledge("user", "lives in");
        second.embedding = Some(vec![0.96, 0.28, 0.0, 0.0]);
        let second_id = store.store_knowledge(&second).unwrap();
        (first_id, second_id)
    };

    let lenient = store();
    let (a, b) = similar(&lenient);
    assert_eq!(a, b, "the 0.95 default must merge cos 0.96");

    let strict = store();
    strict
        .apply_quality_config(&MemoryQualityConfig {
            dedup: acowork_memory::quality::DedupQuality {
                knowledge_threshold: 0.99,
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
    let (a, b) = similar(&strict);
    assert_ne!(a, b, "a raised threshold must not merge cos 0.96");
    assert_eq!(strict.node_count_by_label(labels::KNOWLEDGE).unwrap(), 2);
}

/// Confirmation hooks are still no-ops for the SQLite provider.
#[test]
fn provider_confirmation_hooks_are_noops() {
    let store = store();
    store.store_episode(&episode("seed")).unwrap();

    assert!(!store.should_trigger_confirmation().unwrap());
    assert_eq!(store.generate_confirmation_hint().unwrap(), None);
}

/// Health and stats must reflect the real store, and stats must not count
/// episodic nodes twice (they are reported separately from semantic nodes).
#[test]
fn provider_stats_and_health() {
    let store = store();
    assert!(store.health_check().unwrap().is_healthy);
    assert!(store.health_check().unwrap().latency_ms < 5_000);

    store.store_episode(&episode("one")).unwrap();
    let mut kn = knowledge("user", "likes");
    kn.status = NodeStatus::Dormant;
    store.store_knowledge(&kn).unwrap();

    let stats = store.stats().unwrap();
    assert_eq!(stats.episode_count, 1);
    assert_eq!(stats.node_count, 1);
    assert_eq!(stats.active_node_count, 0);
    assert_eq!(stats.dormant_node_count, 1);
    assert_eq!(stats.edge_count, 0);
    assert_eq!(stats.index_count, 5);
    assert!(stats.storage_size_bytes > 0);
}

/// `collaboration_span` feeds the 30-day Relationship generation; it must be
/// `None` on an empty store and report the earliest episode timestamp once
/// there is history.
#[test]
fn provider_collaboration_span() {
    let store = store();
    assert!(store.collaboration_span().unwrap().is_none());

    let mut older = episode("first contact");
    older.timestamp = t(1_600_000_000);
    store.store_episode(&older).unwrap();
    store.store_episode(&episode("later")).unwrap();

    let span = store.collaboration_span().unwrap().unwrap();
    assert_eq!(span.earliest_episode_at, t(1_600_000_000));
    assert_eq!(span.episode_count, 2);
}

/// `search_episodes` / `hybrid_search` are the `MemoryQuery` entry points; they
/// must return hits with real content and node ids.
#[test]
fn provider_search_episodes_and_hybrid_search() {
    let store = store();
    let mut ep = episode("北京今天下雨吗");
    ep.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let id = store.store_episode(&ep).unwrap();
    store.store_knowledge(&knowledge("user", "likes")).unwrap();

    let mut query = MemoryQuery::deep_recall("北京今天下雨吗".to_string(), None);
    query.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);

    let episodes = store.search_episodes(&query).unwrap();
    assert_eq!(episodes.len(), 1);
    assert_eq!(episodes[0].node_id, id);
    assert_eq!(episodes[0].content, "北京今天下雨吗");

    let all = store.hybrid_search(&query).unwrap();
    assert!(all.iter().any(|hit| hit.node_id == id));
    assert!(all.iter().all(|hit| !hit.content.is_empty()));
}

// ── MemoryAdminService (ADR-082 D1) ──────────────────────────────────────
//
// The admin surface is what the Desktop Memory panel and the HTTP admin
// endpoints call, so these tests pin the wire contract: pagination, filters,
// the eager `rejected_unfiltered` guard, and the record/detail field meaning.

use acowork_memory::MemoryAdminService;
use acowork_memory::admin::{AdminListNodesParams, RebuildScope, RebuildStats};
use serde_json::json;

/// Every filter the panel can send must actually narrow the scan.
#[test]
fn admin_list_nodes_filters_and_paginates() {
    let store = store();

    let mut recent = episode("recent rain");
    recent.timestamp = Utc::now();
    recent.knowledge_subtype = Some(KnowledgeSubType::Preference);
    let recent_id = store.store_episode(&recent).unwrap();

    let mut old = episode("old rain");
    old.timestamp = Utc::now() - TimeDelta::days(30);
    let old_id = store.store_episode(&old).unwrap();

    let mut node = knowledge("user", "likes");
    node.object = "oolong tea".to_string();
    node.sub_type = KnowledgeSubType::Fact;
    // Recent, so the time-range assertion below only exercises the window.
    node.created_at = Utc::now();
    node.updated_at = Utc::now();
    let knowledge_id = store.store_knowledge(&node).unwrap();

    let page = |params: AdminListNodesParams| store.list_nodes(&params);

    // Unfiltered: newest first (created_at DESC).
    let all = page(AdminListNodesParams {
        size: 50,
        ..Default::default()
    });
    assert_eq!(all.total, 3);
    assert_eq!(all.nodes.len(), 3);
    assert!(all.rejected_unfiltered.is_none());
    let timestamps: Vec<i64> = all.nodes.iter().map(|n| n.created_at).collect();
    assert!(
        timestamps.windows(2).all(|w| w[0] >= w[1]),
        "list must be newest first: {timestamps:?}"
    );
    assert_eq!(all.nodes.last().unwrap().node_id, old_id);

    // Pagination is contiguous: page 1 + page 2 cover every node exactly once.
    let first = page(AdminListNodesParams {
        page: 1,
        size: 2,
        ..Default::default()
    });
    let second = page(AdminListNodesParams {
        page: 2,
        size: 2,
        ..Default::default()
    });
    assert_eq!(first.nodes.len(), 2);
    assert_eq!(second.nodes.len(), 1);
    let mut seen: Vec<u64> = first
        .nodes
        .iter()
        .chain(second.nodes.iter())
        .map(|n| n.node_id)
        .collect();
    seen.sort_unstable();
    let mut expected = vec![old_id, knowledge_id, recent_id];
    expected.sort_unstable();
    assert_eq!(seen, expected, "pages must not repeat or drop rows");

    // node_type filter.
    let knowledge_only = page(AdminListNodesParams {
        size: 50,
        node_type: labels::KNOWLEDGE.to_string(),
        ..Default::default()
    });
    assert_eq!(knowledge_only.total, 1);
    assert_eq!(knowledge_only.nodes[0].node_id, knowledge_id);

    // keyword filter (case-insensitive substring over rendered content).
    let tea = page(AdminListNodesParams {
        size: 50,
        keyword: "OOLONG".to_string(),
        ..Default::default()
    });
    assert_eq!(tea.total, 1);
    assert_eq!(tea.nodes[0].content, "user likes oolong tea");

    // sub_type filter, per label (Knowledge → `sub_type`).
    let facts = page(AdminListNodesParams {
        size: 50,
        node_type: labels::KNOWLEDGE.to_string(),
        sub_type: "Fact".to_string(),
        ..Default::default()
    });
    assert_eq!(facts.total, 1);
    let prefs = page(AdminListNodesParams {
        size: 50,
        node_type: labels::KNOWLEDGE.to_string(),
        sub_type: "Preference".to_string(),
        ..Default::default()
    });
    assert_eq!(prefs.total, 0);

    // Episodic carries its sub-type in `knowledge_subtype`.
    let episodic_pref = page(AdminListNodesParams {
        size: 50,
        node_type: labels::EPISODIC.to_string(),
        sub_type: "Preference".to_string(),
        ..Default::default()
    });
    assert_eq!(episodic_pref.total, 1);

    // time_range window drops the 30-day-old episode.
    let week = page(AdminListNodesParams {
        size: 50,
        time_range: "7d".to_string(),
        ..Default::default()
    });
    assert_eq!(week.total, 2);
    assert!(!week.nodes.iter().any(|n| n.node_id == old_id));

    // A size of 0 clamps up rather than returning an empty page forever.
    let clamped = page(AdminListNodesParams {
        size: 0,
        ..Default::default()
    });
    assert_eq!(clamped.size, 1);
    assert_eq!(clamped.nodes.len(), 1);
}

/// An unfiltered scan of an oversized store is refused eagerly instead of
/// materializing every node; a filtered scan is still allowed.
#[test]
fn admin_list_rejects_unfiltered_oversized_scan() {
    let store = store();
    {
        let conn = store.lock();
        // Bulk insert to keep the test fast: the guard only counts rows.
        for i in 0..10_001 {
            conn.execute(
                "INSERT INTO nodes(label, status, props, created_at, updated_at) VALUES (?1, 'Active', ?2, ?3, ?3)",
                params![
                    labels::EPISODIC,
                    format!("{{\"content\":\"filler {i}\"}}"),
                    ts_text(Utc::now())
                ],
            )
            .unwrap();
        }
    }

    let rejected = store.list_nodes(&AdminListNodesParams {
        size: 10,
        ..Default::default()
    });
    assert_eq!(rejected.rejected_unfiltered, Some(10_001));
    assert!(rejected.nodes.is_empty());
    assert_eq!(rejected.total, 10_001);

    let filtered = store.list_nodes(&AdminListNodesParams {
        size: 10,
        keyword: "filler 1".to_string(),
        ..Default::default()
    });
    assert!(filtered.rejected_unfiltered.is_none());
    assert!(!filtered.nodes.is_empty());
}

/// `semantic_search` mode selects the retrieval path, and a query with neither
/// text nor embedding is a no-op.
#[test]
fn admin_semantic_search_modes() {
    let store = store();
    let mut ep = episode("北京今天下雨吗");
    ep.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let id = store.store_episode(&ep).unwrap();

    let mut other = episode("totally different words");
    other.embedding = Some(vec![0.0, 1.0, 0.0, 0.0]);
    store.store_episode(&other).unwrap();

    let empty = store.semantic_search("", None, "keyword", 10);
    assert!(empty.is_empty());

    let keyword = store.semantic_search("北京今天下雨吗", None, "keyword", 10);
    assert_eq!(keyword.len(), 1);
    assert_eq!(keyword[0].node_id, id);

    let vector = store.semantic_search("", Some(&[1.0, 0.0, 0.0, 0.0]), "vector", 10);
    assert_eq!(vector[0].node_id, id, "exact match must rank first");

    let hybrid = store.semantic_search("北京今天下雨吗", Some(&[1.0, 0.0, 0.0, 0.0]), "hybrid", 10);
    assert!(!hybrid.is_empty());
    assert!(hybrid.iter().all(|r| !r.content.is_empty()));

    // No embedding + "vector" mode degrades to text search (embedding model
    // not ready), never to an error.
    let degraded = store.semantic_search("北京今天下雨吗", None, "vector", 10);
    assert_eq!(degraded.len(), 1);
}

/// Detail exposes the raw property map so the panel can show fields the typed
/// API does not model.
#[test]
fn admin_get_node_detail_and_missing() {
    let store = store();
    let mut node = knowledge("user", "likes");
    node.confidence = 0.75;
    node.importance = 0.4;
    let id = store.store_knowledge(&node).unwrap();

    let detail = store.get_node(id);
    assert!(detail.found);
    assert_eq!(detail.node_type, labels::KNOWLEDGE);
    assert_eq!(detail.content, "user likes Beijing");
    assert!((detail.confidence - 0.75).abs() < 1e-6);
    assert!((detail.importance - 0.4).abs() < 1e-6);
    assert_eq!(detail.status, "Active");
    assert_eq!(
        detail.properties.get("subject").and_then(|v| v.as_str()),
        Some("user")
    );
    assert!(detail.created_at > 0);
    assert!(detail.message.is_empty());

    let missing = store.get_node(9_999);
    assert!(!missing.found);
    assert!(missing.properties.is_empty());
    assert!(!missing.message.is_empty());
}

/// Raw CRUD: create from a property map, patch without dropping other fields,
/// and refuse to patch a node that does not exist.
#[test]
fn admin_create_update_delete_node() {
    let store = store();

    let properties = HashMap::from([
        ("subject".to_string(), json!("user")),
        ("predicate".to_string(), json!("drinks")),
        ("object".to_string(), json!("tea")),
        ("status".to_string(), json!("Active")),
        ("confidence".to_string(), json!(0.9)),
        ("embedding".to_string(), json!([1.0, 0.0, 0.0, 0.0])),
    ]);
    let id = store
        .create_node(labels::KNOWLEDGE, &properties)
        .expect("create_node");

    let detail = store.get_node(id);
    assert_eq!(detail.content, "user drinks tea");
    assert_eq!(detail.status, "Active");
    // `embedding` lives in the `vectors` table, never duplicated into `props`.
    assert!(!detail.properties.contains_key("embedding"));
    assert!(detail.properties.contains_key("subject"));
    assert_eq!(MemoryAdminService::count_nodes_with_embedding(&store), 1);

    // Patching one property must preserve the rest (merge, not replace).
    store
        .update_node(
            id,
            &HashMap::from([("object".to_string(), json!("coffee"))]),
        )
        .expect("update_node");
    let patched = store.get_node(id);
    assert_eq!(patched.content, "user drinks coffee");
    assert_eq!(
        patched.properties.get("subject").and_then(|v| v.as_str()),
        Some("user")
    );

    // Updating a missing node is an error, so the HTTP layer can 404.
    assert!(
        store
            .update_node(9_999, &HashMap::from([("object".to_string(), json!("x"))]))
            .is_err()
    );

    assert!(MemoryAdminService::delete_node(&store, id));
    assert!(
        !MemoryAdminService::delete_node(&store, id),
        "second delete reports not-found"
    );
    assert!(!store.get_node(id).found);
}

/// Stats must agree with the underlying counts and surface the purge log.
#[test]
fn admin_stats_shape() {
    let store = store();
    store.store_episode(&episode("one")).unwrap();
    let mut dormant = knowledge("user", "likes");
    dormant.status = NodeStatus::Dormant;
    store.store_knowledge(&dormant).unwrap();

    let stats = store.get_stats();
    assert_eq!(stats.total_nodes, 2);
    assert_eq!(stats.by_type.get(labels::EPISODIC), Some(&1));
    assert_eq!(stats.by_type.get(labels::KNOWLEDGE), Some(&1));
    assert_eq!(stats.by_status.get("Dormant"), Some(&1));
    assert_eq!(stats.by_status.get("purged"), Some(&0));
    assert_eq!(stats.stored_dim, DIM as u64);
    assert_eq!(stats.nodes_with_embedding, 0);
    assert_eq!(stats.vectors_of_other_dim, 0);
    assert!(stats.storage_bytes > 0);
    // Two nodes, no vectors. `index_health` used to be the literal "healthy"
    // regardless of what the counters said, so this is the case that proves it
    // is now derived from them.
    assert_eq!(stats.index_health, "missing_embeddings");
}

/// The health string must distinguish "a node has no vector" from "a node has a
/// vector of a width the store no longer declares".
///
/// Only the first was ever reportable, and only by luck: `vector_search` skips
/// foreign-width rows silently, so a store whose dimension was moved over
/// vectors still at the previous width looks complete to every count of
/// *missing* embeddings while being half invisible to retrieval.
#[test]
fn admin_stats_reports_foreign_width_vectors() {
    let store = store();
    let mut a = episode("first");
    a.embedding = Some(emb(0.1));
    let mut b = episode("second");
    b.embedding = Some(emb(0.9));
    store.store_episode(&a).unwrap();
    store.store_episode(&b).unwrap();

    let stats = store.get_stats();
    assert_eq!(stats.total_nodes, 2);
    assert_eq!(stats.nodes_with_embedding, 2);
    assert_eq!(stats.index_health, "healthy", "every node has a vector");

    // A model swap flips the declared width; the rows do not move with it.
    store.set_embedding_dim(2).unwrap();
    let stats = store.get_stats();
    assert_eq!(stats.stored_dim, 2);
    assert_eq!(
        stats.nodes_with_embedding, 2,
        "they still have vectors - nothing is 'missing'"
    );
    assert_eq!(stats.vectors_of_other_dim, 2);
    assert_eq!(
        stats.index_health, "stale_vectors",
        "both vectors are now invisible to search"
    );
}

/// `nodes_with_embedding` must be counted over the labels `total_nodes` counts,
/// or the two are not comparable.
///
/// Conversation messages are in the vectors table but not in `labels::ALL`, so
/// counting the whole table made `nodes_with_embedding < total_nodes` false on
/// any store with chat history - which is every store the panel is used on.
#[test]
fn admin_stats_embedding_count_excludes_conversation_rows() {
    let store = std::sync::Arc::new(SqliteStore::open_in_memory(DIM).expect("open"));
    let index = conversation::ConversationStore::from_store(store.clone()).expect("index");
    for line in 0..3 {
        index
            .index_message("s1", line, "user", &format!("message {line}"), &emb(0.2))
            .expect("index message");
    }
    let mut with = episode("has a vector");
    with.embedding = Some(emb(0.4));
    store.store_episode(&with).unwrap();
    store.store_episode(&episode("has none")).unwrap();

    let stats = store.get_stats();
    assert_eq!(stats.total_nodes, 2, "memory nodes only");
    assert_eq!(
        stats.nodes_with_embedding, 1,
        "and its vector count must be over those same 2 nodes, not over all 5 rows"
    );
    assert_eq!(stats.index_health, "missing_embeddings");
}

/// `migrate_embedding_dimension` must re-embed what it can, count what it
/// cannot, and leave the store reporting the new dimension.
#[test]
fn admin_embedding_migration() {
    let store = store();

    let mut mine = episode("migrate me");
    mine.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
    let mine_id = store.store_episode(&mine).unwrap();

    let mut empty = episode("   ");
    empty.content = String::new();
    store.store_episode(&empty).unwrap();

    let mut no_embedding = episode("no model available");
    no_embedding.content = "model returns nothing".to_string();
    store.store_episode(&no_embedding).unwrap();

    // A model that reports the wrong width must be rejected, not written.
    let stats: RebuildStats = store
        .migrate_embedding_dimension(
            &|content: &str| {
                if content.contains("model returns nothing") {
                    None
                } else {
                    Some(vec![0.5, 0.5])
                }
            },
            2,
        )
        .expect("migrate");

    assert_eq!(stats.total_scanned, 3);
    assert_eq!(stats.rebuilt, 1);
    assert_eq!(stats.skipped_no_embedding, 1);
    assert_eq!(stats.skipped_no_content, 1);
    assert_eq!(stats.errors, 0);
    assert_eq!(store.embedding_dim(), 2);
    assert_eq!(MemoryAdminService::count_nodes_with_embedding(&store), 1);

    // The re-embedded node is searchable at the new width.
    let hits = store.semantic_search("migrate me", Some(&[0.5, 0.5]), "hybrid", 10);
    assert!(!hits.is_empty());
    // The node itself is untouched; only its vector was rewritten.
    let detail = store.get_node(mine_id);
    assert!(detail.found);
    assert_eq!(detail.content, "migrate me");
}

/// A wrong-width vector from the embedder is counted as an error rather than
/// silently poisoning the vector index.
#[test]
fn admin_embedding_migration_rejects_wrong_width() {
    let store = store();
    store.store_episode(&episode("anything")).unwrap();

    let stats = store
        .migrate_embedding_dimension(&|_: &str| Some(vec![1.0, 2.0, 3.0]), 2)
        .expect("migrate");

    assert_eq!(stats.errors, 1);
    assert_eq!(stats.rebuilt, 0);
    assert_eq!(MemoryAdminService::count_nodes_with_embedding(&store), 0);
}

/// The stored dimension is part of the database, not of the caller: reopening
/// with a different value must not reinterpret existing vectors.
#[test]
fn admin_embedding_dim_persists_across_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("memory.db");

    {
        let store = SqliteStore::open(&path, 4).expect("open");
        let mut ep = episode("persisted");
        ep.embedding = Some(vec![1.0, 0.0, 0.0, 0.0]);
        store.store_episode(&ep).unwrap();
        store.close().unwrap();
    }

    let reopened = SqliteStore::open(&path, 8).expect("reopen");
    assert_eq!(reopened.embedding_dim(), 4, "stored dimension must win");
    assert_eq!(MemoryAdminService::count_nodes_with_embedding(&reopened), 1);
}

/// A model swap must still heal a store whose declared dimension already
/// matches the target.
///
/// This is the state a live store was left in: the meta had been flipped to the
/// new width before the migration bailed out, the provider has since caught up,
/// and the message rows are still at the old width. Nothing compares the two
/// dimensions any more, so the only way back is the manual rebuild - which
/// reaches this same call with `new_dim == embedding_dim()`. An early bail on
/// matching dimensions would leave those rows permanently invisible.
#[test]
fn admin_embedding_migration_heals_a_store_whose_dim_was_flipped_early() {
    let store = std::sync::Arc::new(SqliteStore::open_in_memory(DIM).expect("open"));
    let index = conversation::ConversationStore::from_store(store.clone()).expect("index");
    for (line, text) in [
        (0, "where does the temp dir point"),
        (1, "the installer is built with wix"),
    ] {
        index
            .index_message("s1", line, "user", text, &emb(0.2 + line as f32))
            .expect("index message");
    }
    // The memory layer made it across to the new width; the conversation layer
    // never did, and the recorded dimension was flipped anyway.
    let mut migrated = episode("already migrated");
    migrated.embedding = Some(vec![0.5, 0.5]);
    store.store_episode(&migrated).expect("store");
    store.set_embedding_dim(2).expect("flip");

    let stats = store
        .migrate_embedding_dimension(&|_| Some(vec![0.5, 0.5]), 2)
        .expect("rebuild after a flip must not bail on matching dimensions");

    assert_eq!(
        stats.rebuilt, 3,
        "the rebuild re-embeds every row in scope, stale or not, got {stats:?}"
    );
    assert_eq!(store.embedding_dim(), 2);
    let hits = store
        .vector_search(conversation::LABEL, &[0.5, 0.5], 10)
        .expect("search");
    assert_eq!(
        hits.len(),
        2,
        "the healed messages must be visible to vector search"
    );
}

/// The model-switch UI polls a progress slot, so the migration must actually
/// fill it. The trait's default implementation discards the callback, which
/// made every rebuild read "0 of 0" for its whole duration - the work was
/// happening, only the numbers were missing.
#[test]
fn admin_embedding_migration_reports_progress() {
    let store = store();
    let mut a = episode("first");
    a.embedding = Some(emb(0.1));
    store.store_episode(&a).unwrap();
    let mut b = episode("   ");
    b.content = String::new();
    store.store_episode(&b).unwrap();
    let mut c = episode("third");
    c.embedding = Some(emb(0.3));
    store.store_episode(&c).unwrap();

    let seen = std::sync::Mutex::new(Vec::new());
    let stats = MemoryAdminService::migrate_embedding_dimension_with_progress(
        &store,
        &|_: &str| Some(vec![0.5, 0.5]),
        2,
        Some(&|done, total| seen.lock().unwrap().push((done, total))),
    )
    .expect("migrate");

    let seen = seen.into_inner().unwrap();
    assert_eq!(
        seen.len(),
        3,
        "one report per row, including the row skipped for having no content"
    );
    assert_eq!(seen[0], (1, 3));
    assert_eq!(
        *seen.last().unwrap(),
        (3, 3),
        "the bar must reach the total, got {seen:?}"
    );
    assert_eq!(stats.total_scanned, 3);
}

/// A model swap must re-embed conversation messages as well as memory nodes.
///
/// The memory-browser scope (`all_rows`) deliberately excludes them so the panel
/// does not list chat transcripts; the migration may not, because the
/// conversation index is append-only by line watermark and never re-embeds an
/// already-indexed row. Leaving the label out stranded the entire conversation
/// index at the previous width, where `vector_search` skips those rows without
/// saying anything - which is how a live store ended up with 294 of 311 message
/// vectors invisible after a switch to a 1024-dim model.
#[test]
fn admin_embedding_migration_covers_conversation_messages() {
    let store = std::sync::Arc::new(SqliteStore::open_in_memory(DIM).expect("open"));
    let index = conversation::ConversationStore::from_store(store.clone()).expect("index");
    index
        .index_message("s1", 0, "user", "where does the temp dir point", &emb(0.2))
        .expect("index message");

    let stats = store
        .migrate_embedding_dimension(&|_| Some(vec![0.5, 0.5]), 2)
        .expect("migrate");

    assert_eq!(
        stats.rebuilt, 1,
        "the indexed message row must be inside the migration's scope"
    );
    assert_eq!(store.embedding_dim(), 2);
    // And it is searchable at the new width, not skipped as foreign.
    let hits = store
        .vector_search(conversation::LABEL, &[0.5, 0.5], 10)
        .expect("search");
    assert_eq!(
        hits.len(),
        1,
        "the re-embedded message must be visible to search"
    );
}

/// The `Repair` scope re-embeds exactly the rows retrieval cannot see -
/// foreign-width or unembedded - and nothing else.
///
/// This is the fix for the "banner says 2 rows, rebuild scans 5831" defect:
/// the panel's stale/missing-vector banner describes a handful of invisible
/// rows in a store whose declared dimension already matches the model, and a
/// full re-embed of every row to heal them is two rows of work bought with
/// thousands of embed calls. Repair selects rows by stored width, so the
/// progress bar and the cost match the diagnosis.
#[test]
fn admin_embedding_repair_scope_touches_only_invisible_rows() {
    let store = store();
    // A row already at the target width: correct, must not be re-embedded.
    let mut healthy = episode("healthy");
    healthy.embedding = Some(emb(0.1));
    store.store_episode(&healthy).unwrap();
    // A foreign-width row: invisible to vector search, must be re-embedded.
    let mut foreign = episode("foreign");
    foreign.embedding = Some(vec![0.5, 0.5]);
    store.store_episode(&foreign).unwrap();
    // A row with no vector at all: also invisible, also must be re-embedded.
    store.store_episode(&episode("unembedded")).unwrap();

    let embedded = std::sync::Mutex::new(Vec::<String>::new());
    let stats = MemoryAdminService::migrate_embedding_dimension_scoped(
        &store,
        &|content: &str| {
            embedded.lock().unwrap().push(content.to_string());
            Some(emb(0.9))
        },
        DIM,
        RebuildScope::Repair,
        None,
    )
    .expect("repair");

    assert_eq!(
        stats.total_scanned, 2,
        "only the foreign-width and unembedded rows are in scope"
    );
    assert_eq!(stats.rebuilt, 2);
    let embedded = embedded.into_inner().unwrap();
    assert!(
        !embedded.iter().any(|c| c == "healthy"),
        "a row already at the target width must not be re-embedded, got {embedded:?}"
    );

    // All three rows are now visible to search at the target width.
    let hits = store.vector_search(labels::EPISODIC, &emb(0.9), 10).expect("search");
    assert_eq!(hits.len(), 3, "the whole store is searchable again");
}

/// Repair covers conversation rows too - they are part of the same invisible
/// set, and the conversation index never re-embeds an indexed row on its own.
#[test]
fn admin_embedding_repair_scope_includes_conversation_rows() {
    let store = std::sync::Arc::new(SqliteStore::open_in_memory(DIM).expect("open"));
    let index = conversation::ConversationStore::from_store(store.clone()).expect("index");
    index
        .index_message("s1", 0, "user", "a foreign-width message", &[0.5, 0.5])
        .expect("index message");

    let stats = MemoryAdminService::migrate_embedding_dimension_scoped(
        &*store,
        &|_| Some(emb(0.9)),
        DIM,
        RebuildScope::Repair,
        None,
    )
    .expect("repair");

    assert_eq!(
        stats.rebuilt, 1,
        "the foreign-width conversation row must be inside the repair scope"
    );
    let hits = store
        .vector_search(conversation::LABEL, &emb(0.9), 10)
        .expect("search");
    assert_eq!(hits.len(), 1, "and searchable after the repair");
}

/// The banner's promised number must be the rebuild's actual row count.
///
/// `rebuild_scope_count` is what the desktop shows as "this rebuild will
/// process N rows" before the click; the progress bar's total afterwards is
/// the rows the scoped migration selected. Both must come from one
/// predicate - a banner saying 2 over a rebuild processing 5 is the same
/// trust breach as the original 5831 incident, just smaller.
#[test]
fn rebuild_scope_count_matches_the_scoped_selection() {
    let store = std::sync::Arc::new(SqliteStore::open_in_memory(DIM).expect("open"));
    let mut healthy = episode("healthy");
    healthy.embedding = Some(emb(0.1));
    store.store_episode(&healthy).unwrap();
    let mut foreign = episode("foreign");
    foreign.embedding = Some(vec![0.5, 0.5]);
    store.store_episode(&foreign).unwrap();
    store.store_episode(&episode("unembedded")).unwrap();
    // A conversation row at a foreign width is in the repair set too - the
    // panel's memory-node counts cannot see it, the scope count must.
    let index = conversation::ConversationStore::from_store(store.clone()).expect("index");
    index
        .index_message("s1", 0, "user", "a foreign-width message", &[0.5, 0.5])
        .expect("index message");

    let repair = MemoryAdminService::rebuild_scope_count(&*store, RebuildScope::Repair, DIM);
    let all = MemoryAdminService::rebuild_scope_count(&*store, RebuildScope::All, DIM);
    assert_eq!(
        repair, 3,
        "foreign-width row + unembedded row + foreign-width conversation row"
    );
    assert_eq!(all, 4, "the all scope covers every row in the migration label set");

    // Same store, same dim: the scoped migration must select exactly the
    // count the banner promised.
    let stats = MemoryAdminService::migrate_embedding_dimension_scoped(
        &*store,
        &|_| Some(emb(0.9)),
        DIM,
        RebuildScope::Repair,
        None,
    )
    .expect("repair");
    assert_eq!(
        stats.total_scanned, repair,
        "the rebuild processed a different number of rows than the banner promised"
    );
}

/// The store may only *claim* a dimension it actually holds everywhere.
///
/// Flipping `embedding_dim` over a half-migrated store is what made the memory
/// panel report "no rebuild needed" while most vectors were still the old width:
/// the declared dimension is the only thing the panel and the retrieval path
/// compare, and `vector_search` hides the mismatch by skipping rows silently.
#[test]
fn admin_embedding_migration_does_not_claim_a_width_the_store_does_not_have() {
    let store = store();
    let mut reachable = episode("re-embed me");
    reachable.embedding = Some(emb(0.3));
    store.store_episode(&reachable).expect("store");
    let mut unreachable = episode("this one the model cannot reach");
    unreachable.embedding = Some(emb(0.7));
    store.store_episode(&unreachable).expect("store");

    // One node re-embeds, the other's embed call fails, so one row is left at
    // the old width. Note this is about the *row*, not about the counters: a
    // node that never had a vector does not block the move.
    let stats = store
        .migrate_embedding_dimension(
            &|content: &str| {
                if content.contains("cannot") {
                    None
                } else {
                    Some(vec![0.5, 0.5])
                }
            },
            2,
        )
        .expect("migrate");
    assert_eq!(stats.rebuilt, 1);
    assert_eq!(
        store.embedding_dim(),
        DIM,
        "a store still holding an old-width row must not report the new width"
    );

    // Once the remaining row can be embedded, the same call finishes the move.
    store
        .migrate_embedding_dimension(&|_| Some(vec![0.5, 0.5]), 2)
        .expect("second migrate");
    assert_eq!(store.embedding_dim(), 2);
}

#[test]
fn purge_conversation_state_drops_sessions_and_conversation_nodes_keeps_memory() {
    use acowork_memory::session_meta::{SessionMeta, SessionMetaStore};
    use acowork_memory::types::Episode;

    let store = store();
    let store_arc = std::sync::Arc::new(store);
    let meta_store = SqliteSessionMetaStore::new(store_arc.clone());
    let conv_store = conversation::ConversationStore::from_store(store_arc.clone())
        .expect("conv store");

    // Three sessions + four indexed messages.
    for sid in ["s1", "s2", "s3"] {
        meta_store
            .upsert(&SessionMeta {
                version: 1,
                session_id: sid.to_string(),
                agent_id: "com.test.x".to_string(),
                created_at: "2025-01-01T00:00:00.000Z".to_string(),
                user_id: None,
                visibility: None,
                title: Some("x".to_string()),
                workspace_id: None,
                model: None,
                provider: None,
                account_id: None,
                reasoning_effort: None,
                temperature: None,
                context_window: None,
                todos: None,
                message_count: 0,
                llm_call_counter: None,
                perf: None,
                model_ratio: None,
                last_compaction_offset: None,
                last_active_at: "2025-01-01T00:00:00.000Z".to_string(),
                tokens: None,
                corrupted: false,
            })
            .unwrap();
    }
    for (sid, idx) in [("s1", 0usize), ("s1", 1), ("s2", 0), ("s3", 0)] {
        conv_store
            .index_message(sid, idx, "user", "hello", &[0.0; DIM])
            .unwrap();
    }
    // Memory node the user curated — must survive.
    store_arc
        .store_episode(&Episode {
            session_id: "s1".to_string(),
            turn_index: 0,
            role: "user".to_string(),
            content: "remembered fact".to_string(),
            embedding: None,
            timestamp: chrono::Utc::now(),
            consolidated: false,
            metadata: Default::default(),
            importance: 0.5,
            knowledge_subtype: None,
            normalized: None,
        })
        .unwrap();

    let purged = store_arc.purge_conversation_state().unwrap();
    assert_eq!(purged, 3, "purge must report the deleted session count");

    // Sessions gone.
    assert_eq!(meta_store.list_recent(usize::MAX).unwrap().len(), 0);
    // Conversation-index nodes gone.
    assert_eq!(
        store_arc.node_count_by_label(conversation::LABEL).unwrap(),
        0
    );
    // Memory node preserved.
    assert_eq!(store_arc.node_count_by_label("Episodic").unwrap(), 1);
}

#[test]
fn purge_conversation_state_is_a_noop_on_an_empty_store() {
    // The fresh-clone path (no conversation history yet) calls
    // `purge_conversation_state` unconditionally; an empty store must not
    // error or surface rows that were never there.
    let store = store();
    let purged = store.purge_conversation_state().unwrap();
    assert_eq!(purged, 0);
    assert_eq!(store.node_count_by_label(conversation::LABEL).unwrap(), 0);
}
