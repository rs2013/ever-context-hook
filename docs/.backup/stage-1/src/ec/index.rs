//! 代理版全文索引（内嵌 Tantivy，一个二进制，无 sidecar）。
//!
//! ## 设计（对齐设计文档「代理版 Tantivy schema」）
//!
//! 原文权威是 JSONL，索引只存**摘要层**（压缩块）：
//! - doc_type 只有 `ec_block`（会话压缩块）与 `knowledge`（跨会话知识）两类。
//! - 无 ec_raw（原文在 JSONL）、无 ec_backlog/temp（段就是段，只是追赶前缺 title/summary）。
//! - session_id 是独立字段（非 doc_key 前缀 hack）。
//! - message_id = 分段的 ec 挂载点（最后一条消息 id），分支过滤锚。
//!
//! ## 检索（方案 B：粗筛 + 内存过滤）
//!
//! Tantivy 只做全文粗筛（session_id + doc_type + 自由文本），返回候选；
//! 分支精确过滤（message_id ∈ 祖先链集合）由 recall.rs 在内存里做。
//!
//! ## 生命周期
//!
//! 每次 open → 读写 → commit → 退出（一请求一进程，开销已验证可接受）。

use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, QueryParser, TermQuery};
use tantivy::schema::{
    IndexRecordOption, Schema, TantivyDocument, TextFieldIndexing, TextOptions, Value, STORED,
    STRING,
};
use tantivy::tokenizer::{
    LowerCaser, PreTokenizedStream, PreTokenizedString, TextAnalyzer, Token, Tokenizer,
};
use tantivy::{Index, Term};

pub use tantivy::Index as TantivyIndex;

/// 代理版 schema 版本（与 Pi 版 ec-index 的 v4 完全分开，独立演进）。
pub const SCHEMA_VERSION: u32 = 1;
/// CJK 分词器名（与 ec-index 的 ec_cjk_unigram_v1 同实现）。
pub const CJK_TOKENIZER: &str = "ec_cjk_unigram_v1";

/// 构建代理版 schema：摘要层（标题+摘要+定位）+ 会话/分支维度。
pub fn build_schema() -> Schema {
    let mut builder = Schema::builder();
    let exact = STRING | STORED; // raw 分词 + term/前缀查询 + 存储

    builder.add_text_field("session_id", exact.clone()); // 会话标识（首条 user SHA）
    builder.add_text_field("message_id", exact.clone()); // 分段 ec 挂载点（分支过滤锚，仅 STORED 也够，但留索引备用）
    builder.add_text_field("doc_key", exact.clone()); // 唯一键 = session_id:message_id
    builder.add_text_field("doc_type", exact.clone()); // ec_block | knowledge

    let searchable = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(CJK_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();
    builder.add_text_field("title", searchable.clone());
    builder.add_text_field("summary", searchable);

    builder.add_text_field("path", exact); // JSONL 文件路径
    builder.add_u64_field("start_line", STORED); // 段在 JSONL 的起始行
    builder.add_u64_field("end_line", STORED); // 段在 JSONL 的结束行
    builder.build()
}

/// 打开（不存在则创建）索引目录，校验 schema 并注册分词器。
/// schema 不符时清空重建（索引是派生缓存，可从 JSONL 重扫）。
pub fn open_index(dir: &Path) -> Result<Index, Box<dyn std::error::Error>> {
    let meta = dir.join("meta.json");
    let index = if meta.exists() {
        match Index::open_in_dir(dir) {
            Ok(i) => i,
            Err(_) => {
                std::fs::remove_dir_all(dir)?;
                std::fs::create_dir_all(dir)?;
                Index::create_in_dir(dir, build_schema())?
            }
        }
    } else {
        std::fs::create_dir_all(dir)?;
        Index::create_in_dir(dir, build_schema())?
    };
    if index.schema() != build_schema() {
        std::fs::remove_dir_all(dir)?;
        std::fs::create_dir_all(dir)?;
        let i = Index::create_in_dir(dir, build_schema())?;
        register_tokenizer(&i);
        return Ok(i);
    }
    register_tokenizer(&index);
    Ok(index)
}

/// 注册 CJK 分词器（写入与查询两侧一致）。
pub fn register_tokenizer(index: &Index) {
    index.tokenizers().register(
        CJK_TOKENIZER,
        TextAnalyzer::builder(CjkUnigramTokenizer)
            .filter(LowerCaser)
            .build(),
    );
}

/// 一个索引文档（一个压缩段 / 一条跨会话知识）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EcBlockDoc {
    /// 会话标识（首条 user SHA）；knowledge 类型可为空或独立命名空间。
    pub session_id: String,
    /// 分段的 ec 挂载点（最后一条消息的 message_id）；knowledge 类型可空。
    pub message_id: String,
    /// 唯一键 = session_id:message_id（或 knowledge:<id>）。
    pub doc_key: String,
    pub doc_type: String, // "ec_block" | "knowledge"
    pub title: String,    // 追赶前可为空
    pub summary: String,  // 追赶前可为空
    pub path: String,     // JSONL 文件路径
    pub start_line: u64,
    pub end_line: u64,
}

/// 检索命中（含分支过滤所需的 message_id）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub doc_key: String,
    pub doc_type: String,
    pub session_id: String,
    pub message_id: String,
    pub title: String,
    pub summary: String,
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub score: f32,
}

/// 构造索引文档（schema → TantivyDocument）。
fn build_document(schema: &Schema, doc: &EcBlockDoc) -> tantivy::Result<TantivyDocument> {
    let mut d = TantivyDocument::default();
    let f = |name: &str| schema.get_field(name).expect("schema field must exist");
    d.add_text(f("session_id"), &doc.session_id);
    d.add_text(f("message_id"), &doc.message_id);
    d.add_text(f("doc_key"), &doc.doc_key);
    d.add_text(f("doc_type"), &doc.doc_type);
    d.add_text(f("title"), &doc.title);
    d.add_text(f("summary"), &doc.summary);
    d.add_text(f("path"), &doc.path);
    d.add_u64(f("start_line"), doc.start_line);
    d.add_u64(f("end_line"), doc.end_line);
    Ok(d)
}

/// 幂等 upsert：同 doc_key 先删旧再写新。
pub fn index_block(index: &Index, doc: &EcBlockDoc) -> Result<(), Box<dyn std::error::Error>> {
    let schema = index.schema();
    let doc_key_field = schema.get_field("doc_key")?;
    let mut writer: tantivy::IndexWriter<TantivyDocument> = index.writer(50_000_000)?;
    writer.delete_term(Term::from_field_text(doc_key_field, &doc.doc_key));
    writer.add_document(build_document(&schema, doc)?)?;
    writer.commit()?;
    Ok(())
}

/// 批量 upsert（新对话分段建索引时用，单 commit）。
pub fn index_blocks(index: &Index, docs: &[EcBlockDoc]) -> Result<usize, Box<dyn std::error::Error>> {
    let schema = index.schema();
    let doc_key_field = schema.get_field("doc_key")?;
    let mut writer: tantivy::IndexWriter<TantivyDocument> = index.writer(150_000_000)?;
    for doc in docs {
        writer.delete_term(Term::from_field_text(doc_key_field, &doc.doc_key));
        writer.add_document(build_document(&schema, doc)?)?;
    }
    writer.commit()?;
    Ok(docs.len())
}

/// 全文粗筛：session_id 精确 + doc_type 精确 + 自由文本（title+summary）。
///
/// 分支精确过滤（message_id ∈ 祖先链）由调用方在内存里做。
pub fn search(
    index: &Index,
    session_id: &str,
    doc_type: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, Box<dyn std::error::Error>> {
    let schema = index.schema();
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let title = schema.get_field("title")?;
    let summary = schema.get_field("summary")?;
    let session_field = schema.get_field("session_id")?;
    let type_field = schema.get_field("doc_type")?;

    // 自由文本查询（title+summary）
    let text_query: Box<dyn Query> = if query.trim().is_empty() {
        Box::new(tantivy::query::AllQuery)
    } else {
        let parser = QueryParser::for_index(index, vec![title, summary]);
        Box::new(parser.parse_query(query)?)
    };

    // session_id 精确 + doc_type 精确 + 自由文本
    let mut parts: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];
    if !session_id.is_empty() {
        parts.push((Occur::Must, Box::new(TermQuery::new(
            Term::from_field_text(session_field, session_id),
            IndexRecordOption::Basic,
        ))));
    }
    if !doc_type.is_empty() {
        parts.push((Occur::Must, Box::new(TermQuery::new(
            Term::from_field_text(type_field, doc_type),
            IndexRecordOption::Basic,
        ))));
    }

    let bq = BooleanQuery::new(parts);
    let top = searcher.search(&bq, &TopDocs::with_limit(limit.max(1)))?;
    let mut hits = Vec::with_capacity(top.len());
    for (score, addr) in top {
        let doc = searcher.doc(addr)?;
        hits.push(extract_hit(&schema, &doc, score));
    }
    Ok(hits)
}

/// 把会话 JSONL 按增量压缩原则分段，每段建一条索引文档（title/summary 空）。
///
/// 分段：按 target_tokens 累计，在消息边界切（简化版，不处理工具链闭合，后续补）。
/// 每段：message_id = 段最后一条记录 id（ec 挂载点），start_line/end_line = JSONL 行号。
///
/// 幂等：doc_key = session_id:message_id，重复调用会重写同段（先删旧再写新）。
pub fn index_session(
    index: &Index,
    store: &crate::ec::store::Store,
    session_id: &str,
    target_tokens: u32,
) -> Result<usize, Box<dyn std::error::Error>> {
    let rows = store.read_session_with_lines(session_id)?;
    if rows.is_empty() {
        return Ok(0);
    }
    let path = store.session_file_path(session_id).display().to_string();

    // 分段：按 target_tokens 累计，段内至少一条
    let mut blocks: Vec<EcBlockDoc> = Vec::new();
    let mut start = 0usize;
    let mut acc = 0u32;
    for i in 0..rows.len() {
        let cost = estimate_record_tokens(&rows[i].1);
        if acc + cost > target_tokens && i > start {
            // 当前段 [start, i-1] 满，收口
            blocks.push(make_block(session_id, &path, &rows, start, i - 1));
            start = i;
            acc = 0;
        }
        acc += cost;
    }
    // 最后一段 [start, end]
    blocks.push(make_block(session_id, &path, &rows, start, rows.len() - 1));

    // 批量建索引
    index_blocks(index, &blocks)
}

fn make_block(
    session_id: &str,
    path: &str,
    rows: &[(u64, crate::ec::types::MessageRecord)],
    start: usize,
    end: usize,
) -> EcBlockDoc {
    let last = &rows[end].1;
    EcBlockDoc {
        session_id: session_id.to_string(),
        message_id: last.id.clone(), // ec 挂载点 = 段最后一条
        doc_key: format!("{session_id}:{}", last.id),
        doc_type: "ec_block".to_string(),
        title: String::new(),   // 追赶前空
        summary: String::new(), // 追赶前空
        path: path.to_string(),
        start_line: rows[start].0,
        end_line: rows[end].0,
    }
}

fn estimate_record_tokens(r: &crate::ec::types::MessageRecord) -> u32 {
    // 跳过 recall 召回内容：不计入 token（防死循环：召回→膨胀→触发压缩→再召回）
    if r.name.as_deref() == Some("ec_recall_result") {
        return 0;
    }
    let mut t = 4u32;
    t += (r.content.len() as f64 / 3.0).ceil() as u32;
    for tc in &r.tool_calls {
        t += 10;
        t += (tc.function.arguments.len() as f64 / 3.0).ceil() as u32;
    }
    t
}

fn extract_hit(schema: &Schema, doc: &TantivyDocument, score: f32) -> SearchHit {
    let get = |name: &str| -> String {
        schema
            .get_field(name)
            .ok()
            .and_then(|f| doc.get_first(f))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let get_u64 = |name: &str| -> u64 {
        schema
            .get_field(name)
            .ok()
            .and_then(|f| doc.get_first(f))
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    };
    SearchHit {
        doc_key: get("doc_key"),
        doc_type: get("doc_type"),
        session_id: get("session_id"),
        message_id: get("message_id"),
        title: get("title"),
        summary: get("summary"),
        path: get("path"),
        start_line: get_u64("start_line"),
        end_line: get_u64("end_line"),
        score,
    }
}

// ── CJK 分词器（与 ec-index 的 ec_cjk_unigram_v1 同实现） ──

/// 汉字逐字切词，非汉字的连续字母/数字保留为词；不是语义中文分词。
#[derive(Clone)]
struct CjkUnigramTokenizer;

impl Tokenizer for CjkUnigramTokenizer {
    type TokenStream<'a> = PreTokenizedStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let mut tokens = Vec::new();
        let mut word_start = None;
        for (offset, ch) in text.char_indices() {
            if is_han(ch) {
                if let Some(start) = word_start.take() {
                    push_token(&mut tokens, text, start, offset);
                }
                push_token(&mut tokens, text, offset, offset + ch.len_utf8());
            } else if ch.is_alphanumeric() {
                word_start.get_or_insert(offset);
            } else if let Some(start) = word_start.take() {
                push_token(&mut tokens, text, start, offset);
            }
        }
        if let Some(start) = word_start {
            push_token(&mut tokens, text, start, text.len());
        }
        PreTokenizedString {
            text: text.to_owned(),
            tokens,
        }
        .into()
    }
}

fn push_token(tokens: &mut Vec<Token>, text: &str, from: usize, to: usize) {
    tokens.push(Token {
        offset_from: from,
        offset_to: to,
        position: tokens.len(),
        position_length: 1,
        text: text[from..to].to_owned(),
    });
}

fn is_han(ch: char) -> bool {
    matches!(ch as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF |
        0x20000..=0x2A6DF | 0x2A700..=0x2B73F | 0x2B740..=0x2B81F |
        0x2B820..=0x2CEAF | 0x2CEB0..=0x2EBEF | 0x30000..=0x323AF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_index() -> (Index, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("ec-index-test-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        let idx = open_index(&dir).unwrap();
        (idx, dir)
    }

    fn doc(session: &str, mid: &str, title: &str, summary: &str) -> EcBlockDoc {
        EcBlockDoc {
            session_id: session.to_string(),
            message_id: mid.to_string(),
            doc_key: format!("{session}:{mid}"),
            doc_type: "ec_block".to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            path: "/s.jsonl".to_string(),
            start_line: 1,
            end_line: 10,
        }
    }

    #[test]
    fn search_filters_session_and_type() {
        let (idx, dir) = tmp_index();
        index_block(&idx, &doc("s1", "m1", "插件商店设计", "注册分发契约")).unwrap();
        index_block(&idx, &doc("s1", "m2", "代理镜像", "gh-proxy 测速")).unwrap();
        index_block(&idx, &doc("s2", "m1", "插件商店设计", "另一会话的内容")).unwrap();

        // session s1 内搜"插件"
        let hits = search(&idx, "s1", "ec_block", "插件", 10).unwrap();
        assert_eq!(hits.len(), 1); // 只命中 s1 的 m1，不含 s2
        assert_eq!(hits[0].message_id, "m1");

        // 空 query 返回 session 内全部
        let hits = search(&idx, "s1", "ec_block", "", 10).unwrap();
        assert_eq!(hits.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_idempotent() {
        let (idx, dir) = tmp_index();
        index_block(&idx, &doc("s1", "m1", "旧标题", "旧摘要")).unwrap();
        index_block(&idx, &doc("s1", "m1", "新标题", "新摘要")).unwrap();
        let hits = search(&idx, "s1", "ec_block", "", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "新标题");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cjk_tokenizer_splits_han() {
        use tantivy::tokenizer::TokenStream;
        let mut analyzer = TextAnalyzer::builder(CjkUnigramTokenizer)
            .filter(LowerCaser)
            .build();
        let mut stream = analyzer.token_stream("EC上下文");
        let mut tokens = Vec::new();
        stream.process(&mut |t| tokens.push(t.text.clone()));
        assert_eq!(tokens, vec!["ec", "上", "下", "文"]);
    }
}
