ALTER TABLE prompt_audit.audits
    DROP COLUMN key_name,
    DROP COLUMN key_name_observed_at;

DROP TABLE prompt_audit.key_names;
DROP TABLE prompt_audit.key_directory;
