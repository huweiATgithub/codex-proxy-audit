-- 保留既有审计记录与正文，移除输入去重和跨请求会话状态
DROP INDEX prompt_audit.audit_identity;
ALTER TABLE prompt_audit.audits
    DROP COLUMN identity_scope,
    DROP COLUMN identity_key,
    DROP COLUMN identity_basis,
    DROP COLUMN observation_status,
    DROP COLUMN source_position,
    DROP COLUMN policy_version,
    DROP COLUMN context_gap;
DROP TABLE prompt_audit.sessions;
