//! Management projections of immutable audit facts. Collection decisions live elsewhere.
use std::collections::BTreeMap;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, LocalResult, NaiveDate, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use gateway_plugin_sdk::client::HostClient;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::keys::{CurrentKeys, KeyLookupError};
use crate::request_metadata::{RequestMetadata, RequestMetadataError};
use crate::store::{AuditListFilter, Store, StoreError};

const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_QUERY_BYTES: usize = 8192;
const CONTENT_CHARACTERS: usize = 1200;
const CURSOR_LIFETIME_SECONDS: i64 = 24 * 60 * 60;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
struct ApiError {
    status: u16,
    code: &'static str,
    message: &'static str,
}
impl ApiError {
    fn invalid_query() -> Self {
        Self {
            status: 400,
            code: "invalid_query",
            message: "筛选条件无效，请检查日期、关键词和分页参数。",
        }
    }
    fn invalid_cursor() -> Self {
        Self {
            status: 400,
            code: "invalid_cursor",
            message: "分页位置无效或与当前查询不匹配，请重新读取。",
        }
    }
    fn not_found() -> Self {
        Self {
            status: 404,
            code: "not_found",
            message: "记录或内容不存在或已被清理。",
        }
    }
    fn internal() -> Self {
        Self {
            status: 500,
            code: "internal_error",
            message: "审计接口内部错误。",
        }
    }
    fn budget() -> Self {
        Self {
            status: 503,
            code: "unavailable",
            message: "记录超过审计响应大小限制。",
        }
    }
}
impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        if error.is_unavailable() {
            Self {
                status: 503,
                code: "unavailable",
                message: "审计数据库暂时不可用，请稍后重试。",
            }
        } else {
            Self::internal()
        }
    }
}

impl From<KeyLookupError> for ApiError {
    fn from(_: KeyLookupError) -> Self {
        Self {
            status: 503,
            code: "unavailable",
            message: "宿主 Client Key 列表暂时不可用，请稍后重试。",
        }
    }
}

impl From<RequestMetadataError> for ApiError {
    fn from(_: RequestMetadataError) -> Self {
        Self {
            status: 503,
            code: "unavailable",
            message: "宿主请求信息暂时不可用，请稍后重试。",
        }
    }
}

#[derive(Debug)]
pub struct QueryResponse {
    pub status: u16,
    pub value: Value,
}

#[derive(Clone)]
pub struct QueryService {
    store: Store,
    time_zone: Tz,
    cursor_secret: Vec<u8>,
    keys: CurrentKeys,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    time_zone: String,
    scope: String,
    position: Value,
    expires: i64,
}

#[derive(Debug)]
struct ListQuery {
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    key: Option<String>,
    q: String,
    limit: i64,
    cursor: Option<String>,
}

impl QueryService {
    /// A query receives one parsed snapshot of the shared setting.
    /// The signing secret comes from Store::secret, so restarts preserve cursors.
    pub fn new(
        store: Store,
        time_zone: Tz,
        cursor_secret: Vec<u8>,
        keys: CurrentKeys,
    ) -> Result<Self, String> {
        if cursor_secret.len() < 32 {
            return Err("查询服务配置无效。".to_string());
        }
        Ok(Self {
            store,
            time_zone,
            cursor_secret,
            keys,
        })
    }

    pub async fn management(
        store: &Store,
        cursor_secret: Vec<u8>,
        method: &str,
        path: &str,
        raw_query: &str,
        body: &[u8],
        keys: impl std::future::Future<Output = Result<CurrentKeys, KeyLookupError>>,
    ) -> QueryResponse {
        let result = async {
            if method == "PUT" && path == "api/time-zone" {
                parse_params(raw_query, &[])?;
                let time_zone = parse_time_zone(body)?;
                store.save_time_zone(time_zone).await?;
                return Ok(json!({"timeZone": time_zone.name()}));
            }
            if method != "GET" || !body.is_empty() {
                return Err(ApiError::invalid_query());
            }
            let time_zone = store.time_zone().await?;
            let keys = if matches!(path, "api/audits" | "api/audit" | "api/keys") {
                keys.await?
            } else {
                CurrentKeys::default()
            };
            let query = Self::new(store.clone(), time_zone, cursor_secret, keys)
                .map_err(|_| ApiError::internal())?;
            query.dispatch(path, raw_query).await
        }
        .await;
        response(result)
    }

    pub async fn handle(&self, path: &str, raw_query: &str) -> QueryResponse {
        response(self.dispatch(path, raw_query).await)
    }

    /// Optional host metadata is fetched only by its explicit management route.
    pub async fn management_metadata(
        store: &Store,
        host: &HostClient,
        method: &str,
        raw_query: &str,
        body: &[u8],
    ) -> QueryResponse {
        let result = async {
            if method != "GET" || !body.is_empty() {
                return Err(ApiError::invalid_query());
            }
            let params = parse_params(raw_query, &["id"])?;
            let id = required_id(&params, "id")?;
            if id.contains('\0') {
                return Err(ApiError::not_found());
            }
            let audit = store
                .request_reference(id)
                .await?
                .ok_or_else(ApiError::not_found)?;
            // The database read is complete before the host callback starts.
            let metadata =
                RequestMetadata::fetch(host, audit.request_id.as_deref(), &audit.key_id).await?;
            Ok(json!({"auditId": id, "requestMetadata": metadata}))
        }
        .await;
        response(result)
    }

    async fn dispatch(&self, path: &str, raw_query: &str) -> ApiResult<Value> {
        let allowed: &[&str] = match path {
            "api/view-context" => &[],
            "api/audits" => &["from", "to", "key", "q", "limit", "cursor"],
            "api/audit" => &["id"],
            "api/audit-content" => &["id", "contentId", "cursor"],
            "api/keys" => &["cursor"],
            _ => return Err(ApiError::not_found()),
        };
        let params = parse_params(raw_query, allowed)?;
        match path {
            "api/view-context" => Ok(
                json!({"timeZone":self.time_zone.name(),"today":Utc::now().with_timezone(&self.time_zone).format("%Y-%m-%d").to_string()}),
            ),
            "api/audits" => self.list(parse_list(&params)?).await,
            "api/audit" => self.detail(required_id(&params, "id")?).await,
            "api/audit-content" => {
                self.content(
                    required_id(&params, "id")?,
                    required_id(&params, "contentId")?,
                    params.get("cursor").map(String::as_str),
                )
                .await
            }
            "api/keys" => self.keys(params.get("cursor").map(String::as_str)).await,
            _ => unreachable!(),
        }
    }

    fn sign_cursor(&self, scope: &str, position: Value) -> ApiResult<String> {
        let cursor = Cursor {
            version: 2,
            time_zone: self.time_zone.name().to_string(),
            scope: scope.to_string(),
            position,
            expires: Utc::now().timestamp() + CURSOR_LIFETIME_SECONDS,
        };
        let payload = serde_json::to_vec(&cursor).map_err(|_| ApiError::internal())?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.cursor_secret)
            .map_err(|_| ApiError::internal())?;
        mac.update(&payload);
        let value = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        );
        if value.len() > 2048 {
            return Err(ApiError::internal());
        }
        Ok(value)
    }

    fn read_cursor(&self, value: Option<&str>, scope: &str) -> ApiResult<Option<Value>> {
        let Some(value) = value else {
            return Ok(None);
        };
        let (payload, signature) = value.split_once('.').ok_or_else(ApiError::invalid_cursor)?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| ApiError::invalid_cursor())?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| ApiError::invalid_cursor())?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.cursor_secret)
            .map_err(|_| ApiError::internal())?;
        mac.update(&payload);
        mac.verify_slice(&signature)
            .map_err(|_| ApiError::invalid_cursor())?;
        let cursor: Cursor =
            serde_json::from_slice(&payload).map_err(|_| ApiError::invalid_cursor())?;
        if cursor.version != 2 || cursor.time_zone != self.time_zone.name() || cursor.scope != scope
        {
            return Err(ApiError::invalid_cursor());
        }
        if cursor.expires < Utc::now().timestamp() {
            return Err(ApiError {
                status: 410,
                code: "cursor_expired",
                message: "分页位置已失效，请重新读取。",
            });
        }
        if cursor.expires > Utc::now().timestamp() + CURSOR_LIFETIME_SECONDS + 60 {
            return Err(ApiError::invalid_cursor());
        }
        Ok(Some(cursor.position))
    }

    async fn list(&self, query: ListQuery) -> ApiResult<Value> {
        let scope = format!(
            "audits:{}",
            scope_hash(
                &json!({"from":query.from.map(|day|day.to_string()),"to":query.to.map(|day|day.to_string()),"key":query.key,"q":query.q,"limit":query.limit,"timeZone":self.time_zone.name(),"order":"first_observed_at DESC,id COLLATE C DESC"})
            )?
        );
        let position = self.read_cursor(query.cursor.as_deref(), &scope)?;
        let (after_time, after_id) = match position {
            None => (None, None),
            Some(value) => {
                let pair = value
                    .as_array()
                    .filter(|pair| pair.len() == 2)
                    .ok_or_else(ApiError::invalid_cursor)?;
                let timestamp = pair[0].as_str().ok_or_else(ApiError::invalid_cursor)?;
                let timestamp = DateTime::parse_from_rfc3339(timestamp)
                    .map_err(|_| ApiError::invalid_cursor())?
                    .with_timezone(&Utc);
                let id = pair[1]
                    .as_str()
                    .filter(|id| valid_id(id))
                    .ok_or_else(ApiError::invalid_cursor)?;
                (Some(timestamp), Some(id.to_string()))
            }
        };
        let start = query
            .from
            .map(|day| day_boundary(day, self.time_zone))
            .transpose()?;
        let end = query
            .to
            .map(|day| {
                day.succ_opt()
                    .ok_or_else(ApiError::invalid_query)
                    .and_then(|day| day_boundary(day, self.time_zone))
            })
            .transpose()?;
        if query.key.as_deref().is_some_and(|key| key.contains('\0')) {
            return Ok(
                json!({"timeZone":self.time_zone.name(),"items":[],"total":0,"nextCursor":null}),
            );
        }
        // Count and rows share one read-only snapshot within this request. The cursor
        // deliberately does not promise a snapshot across subsequent requests.
        let list = self
            .store
            .list(AuditListFilter {
                start,
                end,
                key: query.key.as_deref(),
                search: &query.q,
                after: after_time.zip(after_id.as_deref()),
                limit: query.limit,
            })
            .await?;
        let total = list.total;
        if !(0..=MAX_SAFE_INTEGER).contains(&total) {
            return Err(ApiError::internal());
        }
        let rows = list.rows;
        let mut items = Vec::new();
        let mut last_position = None;
        for (index, row) in rows.iter().take(query.limit as usize).enumerate() {
            let id = &row.id;
            let time = row.first_observed_at;
            let preview = format!(
                "{}{}{}",
                if row.prefix { "…" } else { "" },
                row.excerpt,
                if row.suffix { "…" } else { "" }
            );
            let context_count = row.context_count;
            if !(0..=MAX_SAFE_INTEGER).contains(&context_count)
                || !valid_id(id)
                || preview.chars().count() > 400
            {
                return Err(ApiError::internal());
            }
            let item = json!({"id":id,"recordedAt":utc_timestamp(time),"recordedAtDisplay":self.display_time(time),"keyId":row.key_id,"keyName":self.keys.name(&row.key_id),"preview":preview,"contextCount":context_count});
            let position = json!([time.to_rfc3339(), id]);
            let next = if rows.len() > index + 1 {
                Some(self.sign_cursor(&scope, position.clone())?)
            } else {
                None
            };
            items.push(item);
            if bytes(
                &json!({"timeZone":self.time_zone.name(),"items":items,"total":total,"nextCursor":next}),
            )? > MAX_RESPONSE_BYTES
            {
                items.pop();
                break;
            }
            last_position = Some(position);
        }
        if items.is_empty() && !rows.is_empty() {
            return Err(ApiError::budget());
        }
        let next_cursor = if rows.len() > items.len() {
            Some(self.sign_cursor(&scope, last_position.ok_or_else(ApiError::internal)?)?)
        } else {
            None
        };
        Ok(
            json!({"timeZone":self.time_zone.name(),"items":items,"total":total,"nextCursor":next_cursor}),
        )
    }

    fn display_time(&self, time: DateTime<Utc>) -> String {
        time.with_timezone(&self.time_zone)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    async fn detail(&self, id: &str) -> ApiResult<Value> {
        if id.contains('\0') {
            return Err(ApiError::not_found());
        }
        let audit = self
            .store
            .detail(id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        let request_id = audit.request_id;
        let key_id = audit.key_id;
        let input_id = audit.input_content_id;
        let mut input = None;
        let mut context = Vec::new();
        for row in audit.contents {
            let content_id = row.id;
            let characters = row.character_count;
            let blocks = row.block_count;
            if !valid_id(&content_id)
                || !(0..=MAX_SAFE_INTEGER).contains(&characters)
                || !(0..=MAX_SAFE_INTEGER).contains(&blocks)
            {
                return Err(ApiError::internal());
            }
            let descriptor =
                json!({"id":content_id,"characterCount":characters,"blockCount":blocks});
            if content_id == input_id {
                input = Some(descriptor);
            } else {
                let role = row.role;
                if !matches!(role.as_deref(), Some("user" | "assistant")) {
                    return Err(ApiError::internal());
                }
                context.push(json!({"id":content_id,"role":role,"phase":bounded_optional(row.phase,80),"truncated":row.truncated,"content":descriptor}));
            }
        }
        let input = input.ok_or_else(ApiError::internal)?;
        let input_kinds = audit.input_kinds;
        if input_kinds.as_array().map(Vec::len)
            != input["blockCount"].as_u64().map(|number| number as usize)
        {
            return Err(ApiError::internal());
        }
        let time = audit.first_observed_at;
        Ok(
            json!({"timeZone":self.time_zone.name(),"id":id,"recordedAt":utc_timestamp(time),"recordedAtDisplay":self.display_time(time),"keyId":key_id,"keyName":self.keys.name(&key_id),"threadId":audit.thread_id,"sessionId":audit.session_id,"requestId":request_id,"turnId":audit.turn_id,"inputKinds":input_kinds,"repositories":audit.repositories,"input":input,"context":context}),
        )
    }

    async fn content(
        &self,
        audit_id: &str,
        content_id: &str,
        cursor: Option<&str>,
    ) -> ApiResult<Value> {
        let scope = format!("content:{}", scope_hash(&json!([audit_id, content_id]))?);
        let position = self
            .read_cursor(cursor, &scope)?
            .unwrap_or_else(|| json!({"index":0,"offset":0}));
        let (index, offset) = parse_content_position(&position)?;
        if audit_id.contains('\0') || content_id.contains('\0') {
            return Err(ApiError::not_found());
        }
        let blocks = self
            .store
            .content(audit_id, content_id)
            .await?
            .ok_or_else(ApiError::not_found)?;
        self.content_page(audit_id, content_id, &scope, &blocks, index, offset)
    }

    fn content_page(
        &self,
        audit_id: &str,
        content_id: &str,
        scope: &str,
        source: &Value,
        mut index: usize,
        mut offset: usize,
    ) -> ApiResult<Value> {
        let source = source.as_array().ok_or_else(ApiError::internal)?;
        if index > source.len() || (index == source.len() && offset != 0) {
            return Err(ApiError::invalid_cursor());
        }
        let mut budget = CONTENT_CHARACTERS;
        let mut segments = Vec::new();
        while index < source.len() && segments.len() < 32 {
            let block = &source[index];
            let next_index;
            let next_offset;
            let segment = match block.get("kind").and_then(Value::as_str) {
                Some("nontext") => {
                    if offset != 0 {
                        return Err(ApiError::invalid_cursor());
                    }
                    let media_type = block
                        .get("mediaType")
                        .and_then(Value::as_str)
                        .filter(|value| value.chars().count() <= 80)
                        .ok_or_else(ApiError::internal)?;
                    let description = block.get("description").cloned().unwrap_or(Value::Null);
                    if !description.is_null()
                        && !description
                            .as_str()
                            .is_some_and(|value| value.chars().count() <= 512)
                    {
                        return Err(ApiError::internal());
                    }
                    next_index = index + 1;
                    next_offset = 0;
                    json!({"kind":"nontext","index":index,"mediaType":media_type,"description":description})
                }
                Some("text") => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(ApiError::internal)?;
                    let total = text.chars().count();
                    if offset > total {
                        return Err(ApiError::invalid_cursor());
                    }
                    if budget == 0 && offset < total {
                        break;
                    }
                    // JSON string escaping can consume six bytes per code point.
                    // Find the largest segment that also fits the plugin response
                    // cap; do not truncate the persisted original.
                    let maximum = (total - offset).min(budget);
                    let mut length = maximum;
                    let mut segment;
                    loop {
                        let part: String = text.chars().skip(offset).take(length).collect();
                        segment = json!({"kind":"text","index":index,"offset":offset,"text":part,"totalCharacters":total});
                        let (next_index, next_offset) = if offset + length == total {
                            (index + 1, 0)
                        } else {
                            (index, offset + length)
                        };
                        let next = if next_index < source.len() {
                            Some(self.sign_cursor(
                                scope,
                                json!({"index":next_index,"offset":next_offset}),
                            )?)
                        } else {
                            None
                        };
                        segments.push(segment.clone());
                        let fits = bytes(
                            &json!({"auditId":audit_id,"contentId":content_id,"blocks":segments,"nextCursor":next}),
                        )? <= MAX_RESPONSE_BYTES;
                        segments.pop();
                        if fits {
                            break;
                        }
                        if length == 0 {
                            break;
                        }
                        length /= 2;
                    }
                    if length == 0 && maximum > 0 {
                        break;
                    }
                    budget -= length;
                    (next_index, next_offset) = if offset + length == total {
                        (index + 1, 0)
                    } else {
                        (index, offset + length)
                    };
                    segment
                }
                _ => return Err(ApiError::internal()),
            };
            let next = if next_index < source.len() {
                Some(self.sign_cursor(scope, json!({"index":next_index,"offset":next_offset}))?)
            } else {
                None
            };
            segments.push(segment);
            if bytes(
                &json!({"auditId":audit_id,"contentId":content_id,"blocks":segments,"nextCursor":next}),
            )? > MAX_RESPONSE_BYTES
            {
                segments.pop();
                break;
            }
            index = next_index;
            offset = next_offset;
            if offset > 0 {
                break;
            }
        }
        if segments.is_empty() && index < source.len() {
            return Err(ApiError::budget());
        }
        let next = if index < source.len() {
            Some(self.sign_cursor(scope, json!({"index":index,"offset":offset}))?)
        } else {
            None
        };
        Ok(json!({"auditId":audit_id,"contentId":content_id,"blocks":segments,"nextCursor":next}))
    }

    async fn keys(&self, cursor: Option<&str>) -> ApiResult<Value> {
        let scope = "keys:host:id ASC";
        let after = self
            .read_cursor(cursor, scope)?
            .map(|value| {
                value
                    .as_str()
                    .filter(|id| valid_id(id))
                    .map(str::to_string)
                    .ok_or_else(ApiError::invalid_cursor)
            })
            .transpose()?;
        let rows = self
            .keys
            .entries()
            .iter()
            .filter(|(id, _)| after.as_ref().is_none_or(|after| *id > after))
            .take(101)
            .collect::<Vec<_>>();
        let mut items = Vec::new();
        let mut last = None;
        for (index, row) in rows.iter().take(100).enumerate() {
            let (id, name) = *row;
            let next = if rows.len() > index + 1 {
                Some(self.sign_cursor(scope, json!(id))?)
            } else {
                None
            };
            items.push(json!({"id":id,"name":name}));
            if bytes(&json!({"items":items,"nextCursor":next}))? > MAX_RESPONSE_BYTES {
                items.pop();
                break;
            }
            last = Some(id.clone());
        }
        if items.is_empty() && !rows.is_empty() {
            return Err(ApiError::budget());
        }
        let next = if rows.len() > items.len() {
            Some(self.sign_cursor(scope, json!(last.ok_or_else(ApiError::internal)?))?)
        } else {
            None
        };
        Ok(json!({"items":items,"nextCursor":next}))
    }
}

fn response(result: ApiResult<Value>) -> QueryResponse {
    let result = result.and_then(|value| {
        if bytes(&value)? > MAX_RESPONSE_BYTES {
            Err(ApiError::budget())
        } else {
            Ok(value)
        }
    });
    match result {
        Ok(value) => QueryResponse { status: 200, value },
        Err(error) => QueryResponse {
            status: error.status,
            value: json!({"error":{"code":error.code,"message":error.message}}),
        },
    }
}

fn parse_time_zone(body: &[u8]) -> ApiResult<Tz> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Setting {
        time_zone: String,
    }
    let invalid = || ApiError {
        status: 400,
        code: "invalid_query",
        message: "审计时区无效，请使用 IANA 时区名称。",
    };
    if body.len() > MAX_QUERY_BYTES {
        return Err(invalid());
    }
    let setting: Setting = serde_json::from_slice(body).map_err(|_| invalid())?;
    if setting.time_zone.len() > 128 {
        return Err(invalid());
    }
    setting.time_zone.parse().map_err(|_| invalid())
}

fn parse_params(raw: &str, allowed: &[&str]) -> ApiResult<BTreeMap<String, String>> {
    if raw.len() > MAX_QUERY_BYTES || raw.starts_with('?') {
        return Err(ApiError::invalid_query());
    }
    let mut values = BTreeMap::new();
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if !allowed.contains(&key.as_ref())
            || values
                .insert(key.into_owned(), value.into_owned())
                .is_some()
        {
            return Err(ApiError::invalid_query());
        }
    }
    if values
        .get("cursor")
        .is_some_and(|value| value.is_empty() || value.chars().count() > 2048)
    {
        return Err(ApiError::invalid_query());
    }
    Ok(values)
}
fn parse_list(params: &BTreeMap<String, String>) -> ApiResult<ListQuery> {
    let from = params
        .get("from")
        .map(|value| parse_date(value))
        .transpose()?;
    let to = params
        .get("to")
        .map(|value| parse_date(value))
        .transpose()?;
    if matches!((from,to),(Some(from),Some(to)) if from>to) {
        return Err(ApiError::invalid_query());
    }
    let key = params
        .get("key")
        .map(|value| {
            if valid_id(value) {
                Ok(value.clone())
            } else {
                Err(ApiError::invalid_query())
            }
        })
        .transpose()?;
    let q = params.get("q").cloned().unwrap_or_default();
    if q.chars().count() > 200 || q != q.trim_matches(ecmascript_whitespace) {
        return Err(ApiError::invalid_query());
    }
    let limit = match params.get("limit").map(String::as_str).unwrap_or("50") {
        "50" => 50,
        "100" => 100,
        "200" => 200,
        _ => return Err(ApiError::invalid_query()),
    };
    Ok(ListQuery {
        from,
        to,
        key,
        q,
        limit,
        cursor: params.get("cursor").cloned(),
    })
}
fn parse_date(value: &str) -> ApiResult<NaiveDate> {
    if value.len() != 10
        || value.as_bytes()[4] != b'-'
        || value.as_bytes()[7] != b'-'
        || value
            .as_bytes()
            .iter()
            .enumerate()
            .any(|(index, byte)| index != 4 && index != 7 && !byte.is_ascii_digit())
    {
        return Err(ApiError::invalid_query());
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| ApiError::invalid_query())
}
fn required_id<'a>(params: &'a BTreeMap<String, String>, field: &str) -> ApiResult<&'a str> {
    params
        .get(field)
        .map(String::as_str)
        .filter(|value| valid_id(value))
        .ok_or_else(ApiError::invalid_query)
}
fn valid_id(value: &str) -> bool {
    (1..=256).contains(&value.chars().count())
}
fn bounded_optional(value: Option<String>, limit: usize) -> Option<String> {
    value.filter(|value| value.chars().count() <= limit)
}
#[cfg(test)]
fn ascii_fold(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_uppercase() {
                character.to_ascii_lowercase()
            } else {
                character
            }
        })
        .collect()
}
fn utc_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}
fn bytes(value: &Value) -> ApiResult<usize> {
    serde_json::to_vec(value)
        .map(|value| value.len())
        .map_err(|_| ApiError::internal())
}
fn scope_hash(value: &Value) -> ApiResult<String> {
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(
        serde_json::to_vec(value).map_err(|_| ApiError::internal())?,
    )))
}
fn parse_content_position(value: &Value) -> ApiResult<(usize, usize)> {
    let object = value
        .as_object()
        .filter(|object| {
            object.len() == 2 && object.contains_key("index") && object.contains_key("offset")
        })
        .ok_or_else(ApiError::invalid_cursor)?;
    let read = |field: &str| {
        object[field]
            .as_u64()
            .filter(|value| *value <= MAX_SAFE_INTEGER as u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(ApiError::invalid_cursor)
    };
    Ok((read("index")?, read("offset")?))
}
// ECMAScript String.trim is the contract's boundary rule, which differs from
// Rust's Unicode whitespace (e.g. U+0085 is not trimmed, U+FEFF is trimmed).
fn ecmascript_whitespace(character: char) -> bool {
    matches!(character,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
}
fn day_boundary(day: NaiveDate, time_zone: Tz) -> ApiResult<DateTime<Utc>> {
    let mut local = day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(ApiError::invalid_query)?;
    // Most dates resolve at midnight. Midnight gaps and entirely skipped local
    // dates map to the first actual instant after the calendar boundary.
    for _ in 0..=172800 {
        match time_zone.from_local_datetime(&local) {
            LocalResult::Single(value) => return Ok(value.with_timezone(&Utc)),
            LocalResult::Ambiguous(first, second) => {
                return Ok(first.min(second).with_timezone(&Utc));
            }
            LocalResult::None => {
                local = local
                    .checked_add_signed(Duration::seconds(1))
                    .ok_or_else(ApiError::invalid_query)?
            }
        }
    }
    Err(ApiError::invalid_query())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service() -> QueryService {
        QueryService::new(
            Store::lazy_memory(),
            "America/New_York".parse().unwrap(),
            vec![7; 32],
            CurrentKeys::default(),
        )
        .unwrap()
    }
    #[test]
    fn timezone_body_is_strict_and_parsed() {
        for body in [
            r#"{}"#,
            r#"{"timeZone":null}"#,
            r#"{"timeZone":"Invalid/Zone"}"#,
            r#"{"timeZone":" UTC"}"#,
            r#"{"timeZone":"UTC","other":1}"#,
            r#"{"timeZone":"UTC","timeZone":"Asia/Tokyo"}"#,
            r#"{"timeZone":"UTC"} trailing"#,
        ] {
            assert_eq!(
                parse_time_zone(body.as_bytes()).unwrap_err().code,
                "invalid_query"
            );
        }
        assert_eq!(
            parse_time_zone(br#"{"timeZone":"Asia/Kathmandu"}"#).unwrap(),
            chrono_tz::Asia::Kathmandu
        );
    }

    #[test]
    fn strict_queries_match_unicode_and_literal_rules() {
        for raw in [
            "q=x&q=y",
            "unknown=1",
            "from=2026-02-30",
            "limit=3",
            "q=%20x",
            "q=%EF%BB%BFx",
        ] {
            let result = parse_params(raw, &["from", "to", "q", "limit"])
                .and_then(|params| parse_list(&params));
            assert!(result.is_err(), "{raw}");
        }
        let mut params = BTreeMap::new();
        params.insert("q".into(), "😀".repeat(200));
        assert!(parse_list(&params).is_ok());
        params.insert("q".into(), "😀".repeat(201));
        assert!(parse_list(&params).is_err());
        params.insert("q".into(), "\u{0085}x".into());
        assert!(parse_list(&params).is_ok());
        assert_eq!(ascii_fold("AÄİΣ_100% \\"), "aÄİΣ_100% \\");
    }
    #[tokio::test]
    async fn cursors_are_authenticated_scoped_persistent_and_expirable() {
        let service = service();
        let cursor = service
            .sign_cursor("a", json!({"index":1,"offset":3}))
            .unwrap();
        assert_eq!(
            service.read_cursor(Some(&cursor), "a").unwrap().unwrap()["offset"],
            3
        );
        let same = QueryService::new(
            service.store.clone(),
            service.time_zone,
            vec![7; 32],
            CurrentKeys::default(),
        )
        .unwrap();
        assert!(same.read_cursor(Some(&cursor), "a").is_ok());
        let other = QueryService::new(
            service.store.clone(),
            chrono_tz::UTC,
            vec![7; 32],
            CurrentKeys::default(),
        )
        .unwrap();
        assert_eq!(
            other.read_cursor(Some(&cursor), "a").unwrap_err().code,
            "invalid_cursor"
        );
        assert_eq!(
            service.read_cursor(Some(&cursor), "b").unwrap_err().code,
            "invalid_cursor"
        );
        let mut tampered = cursor.clone().into_bytes();
        tampered[3] = if tampered[3] == b'a' { b'b' } else { b'a' };
        assert_eq!(
            service
                .read_cursor(Some(std::str::from_utf8(&tampered).unwrap()), "a")
                .unwrap_err()
                .code,
            "invalid_cursor"
        );
        let expired = Cursor {
            version: 2,
            time_zone: service.time_zone.name().to_string(),
            scope: "a".into(),
            position: json!(0),
            expires: Utc::now().timestamp() - 1,
        };
        let payload = serde_json::to_vec(&expired).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&service.cursor_secret).unwrap();
        mac.update(&payload);
        let expired = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        );
        assert_eq!(
            service.read_cursor(Some(&expired), "a").unwrap_err().code,
            "cursor_expired"
        );
    }
    #[test]
    fn calendar_boundaries_account_for_short_long_and_skipped_days() {
        let zone: Tz = "America/New_York".parse().unwrap();
        let boundary = |date| day_boundary(parse_date(date).unwrap(), zone).unwrap();
        assert_eq!(
            (boundary("2026-03-09") - boundary("2026-03-08")).num_hours(),
            23
        );
        assert_eq!(
            (boundary("2026-11-02") - boundary("2026-11-01")).num_hours(),
            25
        );
        let apia: Tz = "Pacific/Apia".parse().unwrap();
        assert_eq!(
            day_boundary(parse_date("2011-12-30").unwrap(), apia).unwrap(),
            day_boundary(parse_date("2011-12-31").unwrap(), apia).unwrap()
        );
    }
    #[tokio::test]
    async fn content_segments_preserve_codepoints_empty_blocks_and_ownership() {
        let service = service();
        let source = json!([{"kind":"text","text":"😀é中".repeat(401)},{"kind":"text","text":""},{"kind":"nontext","mediaType":"image","description":null}]);
        let page = service
            .content_page("a", "c", "content-scope", &source, 0, 0)
            .unwrap();
        assert_eq!(
            page["blocks"][0]["text"].as_str().unwrap().chars().count(),
            1200
        );
        assert_eq!(page["blocks"][0]["totalCharacters"], 1203);
        let position = service
            .read_cursor(page["nextCursor"].as_str(), "content-scope")
            .unwrap()
            .unwrap();
        let (index, offset) = parse_content_position(&position).unwrap();
        assert_eq!((index, offset), (0, 1200));
        let second = service
            .content_page("a", "c", "content-scope", &source, index, offset)
            .unwrap();
        assert_eq!(second["blocks"].as_array().unwrap().len(), 3);
        assert_eq!(second["blocks"][0]["offset"], 1200);
        assert_eq!(second["blocks"][1]["text"], "");
        assert!(second["nextCursor"].is_null());
        assert_eq!(
            service
                .read_cursor(page["nextCursor"].as_str(), "other-owner")
                .unwrap_err()
                .code,
            "invalid_cursor"
        );
    }
    #[tokio::test]
    async fn fixed_response_cap_returns_continuous_content_without_losing_text() {
        let service = service();
        let source = json!([{"kind":"text","text":"\u{0001}😀".repeat(1800)}]);
        let mut index = 0;
        let mut offset = 0;
        let mut combined = String::new();
        loop {
            let page = service
                .content_page("audit", "content", "scope", &source, index, offset)
                .unwrap();
            assert!(bytes(&page).unwrap() <= MAX_RESPONSE_BYTES);
            for block in page["blocks"].as_array().unwrap() {
                combined.push_str(block["text"].as_str().unwrap());
            }
            if page["nextCursor"].is_null() {
                break;
            }
            (index, offset) = parse_content_position(
                &service
                    .read_cursor(page["nextCursor"].as_str(), "scope")
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        }
        assert_eq!(combined, source[0]["text"].as_str().unwrap());
    }
}
