\set ON_ERROR_STOP on
-- Run against the host's existing database as its owner. Supply audit_password
-- through psql's -v option or a protected invocation; this file stores no secret.
\if :{?audit_password}
\else
  \echo 'audit_password is required (psql -v audit_password=...)'
  \quit 1
\endif
\if :{?audit_role}
\else
  \set audit_role prompt_audit
\endif

SELECT format('CREATE ROLE %I LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT PASSWORD %L', :'audit_role', :'audit_password')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = :'audit_role')
\gexec

ALTER ROLE :"audit_role" NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT;
GRANT CONNECT ON DATABASE :"DBNAME" TO :"audit_role";
CREATE SCHEMA IF NOT EXISTS prompt_audit AUTHORIZATION :"audit_role";
ALTER SCHEMA prompt_audit OWNER TO :"audit_role";
REVOKE ALL ON SCHEMA prompt_audit FROM PUBLIC;
GRANT USAGE, CREATE ON SCHEMA prompt_audit TO :"audit_role";
GRANT USAGE ON SCHEMA public TO :"audit_role";

-- No host FK is created. Host retention/Key deletion must never affect the
-- audit facts or be blocked by this plugin.
REVOKE ALL ON public.model_requests FROM :"audit_role";
REVOKE ALL ON public.model_request_observations FROM :"audit_role";
SELECT format('REVOKE SELECT (%s) ON public.model_requests FROM %I', string_agg(quote_ident(attname), ','), :'audit_role')
FROM pg_attribute WHERE attrelid = 'public.model_requests'::regclass AND attnum > 0 AND NOT attisdropped
\gexec
SELECT format('REVOKE SELECT (%s) ON public.model_request_observations FROM %I', string_agg(quote_ident(attname), ','), :'audit_role')
FROM pg_attribute WHERE attrelid = 'public.model_request_observations'::regclass AND attnum > 0 AND NOT attisdropped
\gexec
GRANT SELECT (id, client_api_key_ref, user_agent, requested_model_id, reasoning_effort)
    ON public.model_request_observations TO :"audit_role";
SET prompt_audit.provision_role = :'audit_role';

-- Column grants cannot override privileges obtained through PUBLIC or other
-- role memberships. Refuse a deployment whose role can see additional columns.
DO $$
DECLARE column_name TEXT;
BEGIN
    FOR column_name IN
        SELECT attname FROM pg_attribute
        WHERE attrelid = 'public.model_requests'::regclass
          AND attnum > 0 AND NOT attisdropped
    LOOP
        IF has_column_privilege(current_setting('prompt_audit.provision_role'), 'public.model_requests', column_name, 'SELECT') THEN
            RAISE EXCEPTION 'audit role can read underlying model_requests column %; remove broad grants before installing', column_name;
        END IF;
    END LOOP;
    FOR column_name IN
        SELECT attname FROM pg_attribute
        WHERE attrelid = 'public.model_request_observations'::regclass
          AND attnum > 0 AND NOT attisdropped
          AND attname NOT IN ('id', 'client_api_key_ref', 'user_agent', 'requested_model_id', 'reasoning_effort')
    LOOP
        IF has_column_privilege(current_setting('prompt_audit.provision_role'), 'public.model_request_observations', column_name, 'SELECT') THEN
            RAISE EXCEPTION 'audit role can read unauthorized request observation column %; remove broad grants or role memberships before installing', column_name;
        END IF;
    END LOOP;
    IF has_table_privilege(current_setting('prompt_audit.provision_role'), 'public.model_requests', 'SELECT,INSERT,UPDATE,DELETE,TRUNCATE') THEN
        RAISE EXCEPTION 'audit role has host model_requests privileges; remove broad grants before installing';
    END IF;
    IF has_table_privilege(current_setting('prompt_audit.provision_role'), 'public.model_request_observations', 'INSERT,UPDATE,DELETE') THEN
        RAISE EXCEPTION 'audit role has request observation write privileges; remove broad grants before installing';
    END IF;
END $$;
RESET prompt_audit.provision_role;
