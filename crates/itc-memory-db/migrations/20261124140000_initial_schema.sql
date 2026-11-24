-- Consolidated from 20261123190000_initial_schema.sql
CREATE TABLE projects (
    id UUID PRIMARY KEY, slug TEXT NOT NULL UNIQUE, display_name TEXT NOT NULL,
    description TEXT, status TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE repositories (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE RESTRICT,
    canonical_remote TEXT UNIQUE, display_name TEXT NOT NULL, is_archived BOOLEAN NOT NULL DEFAULT FALSE,
    first_discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(), last_verified_at TIMESTAMPTZ
);
CREATE TABLE repository_locations (
    id UUID PRIMARY KEY, repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    machine_name TEXT NOT NULL, root_path TEXT NOT NULL, git_dir_path TEXT NOT NULL,
    is_present BOOLEAN NOT NULL DEFAULT TRUE, first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(), UNIQUE (machine_name, root_path)
);
CREATE TABLE commits (
    id UUID PRIMARY KEY, repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    sha TEXT NOT NULL, author_name TEXT, author_email TEXT, authored_at TIMESTAMPTZ, committed_at TIMESTAMPTZ,
    subject TEXT, discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(), UNIQUE (repository_id, sha)
);
CREATE TABLE branches (
    id UUID PRIMARY KEY, repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    name TEXT NOT NULL, head_sha TEXT, is_current BOOLEAN NOT NULL DEFAULT FALSE,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(), UNIQUE (repository_id, name)
);
CREATE TABLE source_documents (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    repository_location_id UUID REFERENCES repository_locations(id) ON DELETE SET NULL,
    path TEXT NOT NULL, document_kind TEXT NOT NULL, title TEXT, is_current BOOLEAN NOT NULL DEFAULT TRUE,
    first_discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(), last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repository_location_id, path)
);
CREATE TABLE document_revisions (
    id UUID PRIMARY KEY, source_document_id UUID NOT NULL REFERENCES source_documents(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL, source_commit_sha TEXT, byte_size BIGINT NOT NULL CHECK (byte_size >= 0),
    modified_at TIMESTAMPTZ, discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(), is_current BOOLEAN NOT NULL DEFAULT TRUE,
    UNIQUE (source_document_id, content_hash)
);
CREATE TABLE document_chunks (
    id UUID PRIMARY KEY, document_revision_id UUID NOT NULL REFERENCES document_revisions(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0), heading TEXT, content TEXT NOT NULL,
    start_line INTEGER CHECK (start_line > 0), end_line INTEGER CHECK (end_line >= start_line),
    search_vector TSVECTOR GENERATED ALWAYS AS (to_tsvector('simple', coalesce(heading, '') || ' ' || content)) STORED,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), UNIQUE (document_revision_id, ordinal)
);
CREATE INDEX document_chunks_search_idx ON document_chunks USING GIN (search_vector);
CREATE INDEX source_documents_project_idx ON source_documents (project_id, is_current);
CREATE INDEX document_revisions_document_idx ON document_revisions (source_document_id, is_current);
CREATE TABLE plans (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_document_id UUID REFERENCES source_documents(id) ON DELETE SET NULL, title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active', is_current BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX plans_current_per_project_idx ON plans (project_id) WHERE is_current;
CREATE TABLE phases (
    id UUID PRIMARY KEY, plan_id UUID NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0), title TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), UNIQUE (plan_id, ordinal)
);
CREATE TABLE tasks (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    phase_id UUID REFERENCES phases(id) ON DELETE SET NULL, parent_task_id UUID REFERENCES tasks(id) ON DELETE SET NULL,
    title TEXT NOT NULL, description TEXT, status TEXT NOT NULL DEFAULT 'pending', priority SMALLINT,
    source_reference JSONB NOT NULL DEFAULT '{}'::jsonb, created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), completed_at TIMESTAMPTZ
);
CREATE INDEX tasks_project_status_idx ON tasks (project_id, status);
CREATE TABLE decisions (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_document_id UUID REFERENCES source_documents(id) ON DELETE SET NULL, title TEXT NOT NULL,
    rationale TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'validated', decided_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_verified_at TIMESTAMPTZ, superseded_by_id UUID REFERENCES decisions(id) ON DELETE SET NULL,
    provenance JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX decisions_project_current_idx ON decisions (project_id, decided_at DESC) WHERE superseded_by_id IS NULL;
CREATE TABLE sessions (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    task_id UUID REFERENCES tasks(id) ON DELETE SET NULL, client_kind TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL DEFAULT now(), ended_at TIMESTAMPTZ, summary TEXT,
    provenance JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE TABLE checkpoints (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    session_id UUID REFERENCES sessions(id) ON DELETE SET NULL, task_id UUID REFERENCES tasks(id) ON DELETE SET NULL,
    title TEXT NOT NULL, summary TEXT NOT NULL, repository_state JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX checkpoints_project_created_idx ON checkpoints (project_id, created_at DESC);
CREATE TABLE pull_requests (
    id UUID PRIMARY KEY, repository_id UUID NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    external_number INTEGER NOT NULL CHECK (external_number > 0), title TEXT NOT NULL, state TEXT NOT NULL,
    url TEXT, head_sha TEXT, merged_at TIMESTAMPTZ, last_verified_at TIMESTAMPTZ,
    UNIQUE (repository_id, external_number)
);
CREATE TABLE conventions (
    id UUID PRIMARY KEY, project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    source_document_id UUID REFERENCES source_documents(id) ON DELETE SET NULL, category TEXT NOT NULL,
    rule TEXT NOT NULL, rationale TEXT, status TEXT NOT NULL DEFAULT 'active',
    provenance JSONB NOT NULL DEFAULT '{}'::jsonb, created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, category, rule)
);
CREATE TABLE indexing_runs (
    id UUID PRIMARY KEY, repository_location_id UUID REFERENCES repository_locations(id) ON DELETE SET NULL,
    root_path TEXT NOT NULL, status TEXT NOT NULL, started_at TIMESTAMPTZ NOT NULL DEFAULT now(), finished_at TIMESTAMPTZ,
    discovered_count INTEGER NOT NULL DEFAULT 0 CHECK (discovered_count >= 0),
    indexed_count INTEGER NOT NULL DEFAULT 0 CHECK (indexed_count >= 0),
    failed_count INTEGER NOT NULL DEFAULT 0 CHECK (failed_count >= 0), error_summary TEXT,
    configuration JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX indexing_runs_root_started_idx ON indexing_runs (root_path, started_at DESC);

-- Consolidated from 20261123200000_indexing_metadata.sql
ALTER TABLE repository_locations ADD COLUMN current_branch TEXT;
ALTER TABLE repository_locations ADD COLUMN head_sha TEXT;
ALTER TABLE repository_locations ADD COLUMN is_dirty BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE source_documents ADD COLUMN byte_size BIGINT;
ALTER TABLE source_documents ADD COLUMN mtime_ns BIGINT;

-- Consolidated from 20261123210000_code_intelligence.sql
CREATE TABLE code_symbols (
    id UUID PRIMARY KEY,
    source_document_id UUID NOT NULL REFERENCES source_documents(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    symbol_kind TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    line_number INTEGER NOT NULL CHECK (line_number > 0),
    signature TEXT,
    discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (source_document_id, qualified_name, line_number)
);
CREATE INDEX code_symbols_name_idx ON code_symbols (name);
CREATE INDEX code_symbols_document_idx ON code_symbols (source_document_id);

CREATE TABLE indexing_failures (
    id UUID PRIMARY KEY,
    indexing_run_id UUID NOT NULL REFERENCES indexing_runs(id) ON DELETE CASCADE,
    path TEXT,
    operation TEXT NOT NULL,
    message TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX indexing_failures_run_idx ON indexing_failures (indexing_run_id);

-- Consolidated from 20261123220000_retrieval_completeness.sql
ALTER TABLE source_documents
    ADD COLUMN freshness_status TEXT NOT NULL DEFAULT 'current'
        CHECK (freshness_status IN ('current', 'stale', 'historical', 'temporary', 'duplicate', 'superseded')),
    ADD COLUMN superseded_by_document_id UUID REFERENCES source_documents(id) ON DELETE SET NULL,
    ADD COLUMN duplicate_of_document_id UUID REFERENCES source_documents(id) ON DELETE SET NULL,
    ADD COLUMN symbols_indexed BOOLEAN NOT NULL DEFAULT FALSE;

WITH ranked_revisions AS (
    SELECT id, row_number() OVER (
        PARTITION BY source_document_id
        ORDER BY discovered_at DESC, id
    ) AS revision_rank
    FROM document_revisions
)
UPDATE document_revisions AS revisions
SET is_current = (ranked_revisions.revision_rank = 1)
FROM ranked_revisions
WHERE revisions.id = ranked_revisions.id;

CREATE UNIQUE INDEX document_revisions_one_current_idx
    ON document_revisions (source_document_id)
    WHERE is_current;

CREATE INDEX source_documents_freshness_idx
    ON source_documents (project_id, freshness_status, last_seen_at DESC);

CREATE TABLE repository_dependencies (
    id UUID PRIMARY KEY,
    repository_location_id UUID NOT NULL REFERENCES repository_locations(id) ON DELETE CASCADE,
    source_document_id UUID REFERENCES source_documents(id) ON DELETE CASCADE,
    dependency_name TEXT NOT NULL,
    requirement TEXT NOT NULL,
    dependency_kind TEXT NOT NULL,
    target TEXT NOT NULL DEFAULT '',
    optional BOOLEAN NOT NULL DEFAULT FALSE,
    discovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repository_location_id, source_document_id, dependency_name, dependency_kind, target)
);
CREATE INDEX repository_dependencies_name_idx ON repository_dependencies (dependency_name);

CREATE TABLE repository_changes (
    id UUID PRIMARY KEY,
    repository_location_id UUID NOT NULL REFERENCES repository_locations(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    status_text TEXT NOT NULL,
    is_staged BOOLEAN NOT NULL DEFAULT FALSE,
    is_unstaged BOOLEAN NOT NULL DEFAULT FALSE,
    is_untracked BOOLEAN NOT NULL DEFAULT FALSE,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repository_location_id, path)
);
CREATE INDEX repository_changes_location_idx
    ON repository_changes (repository_location_id, is_untracked, path);

-- Consolidated from 20261123230000_workflow_continuity.sql
ALTER TABLE plans
    ADD COLUMN source_revision_id UUID REFERENCES document_revisions(id) ON DELETE SET NULL;

CREATE UNIQUE INDEX plans_source_document_unique_idx
    ON plans (source_document_id)
    WHERE source_document_id IS NOT NULL;

ALTER TABLE phases
    ADD COLUMN source_line INTEGER CHECK (source_line > 0);

ALTER TABLE tasks
    ADD COLUMN source_document_id UUID REFERENCES source_documents(id) ON DELETE SET NULL,
    ADD COLUMN source_line INTEGER CHECK (source_line > 0),
    ADD COLUMN checkbox_checked BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN status_source TEXT NOT NULL DEFAULT 'agent'
        CHECK (status_source IN ('plan', 'agent'));

CREATE UNIQUE INDEX tasks_source_location_unique_idx
    ON tasks (source_document_id, source_line)
    WHERE source_document_id IS NOT NULL AND source_line IS NOT NULL;

ALTER TABLE projects
    ADD COLUMN current_task_id UUID REFERENCES tasks(id) ON DELETE SET NULL;

ALTER TABLE decisions
    ADD COLUMN source_revision_id UUID REFERENCES document_revisions(id) ON DELETE SET NULL;

ALTER TABLE checkpoints
    ADD COLUMN next_steps TEXT NOT NULL DEFAULT '',
    ADD COLUMN blockers TEXT NOT NULL DEFAULT '';

CREATE TABLE task_references (
    id UUID PRIMARY KEY,
    task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    repository_id UUID REFERENCES repositories(id) ON DELETE SET NULL,
    reference_kind TEXT NOT NULL CHECK (reference_kind IN ('commit', 'pull-request', 'implementation')),
    reference TEXT NOT NULL,
    title TEXT,
    url TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (task_id, reference_kind, reference)
);

CREATE INDEX task_references_task_idx
    ON task_references (task_id, created_at DESC);

-- Consolidated from 20261124100000_model_inference_usage.sql
CREATE TABLE model_inference_runs (
    id UUID PRIMARY KEY,
    task_profile_id TEXT,
    model_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    provider_kind TEXT NOT NULL CHECK (provider_kind IN ('ollama', 'open_ai_compatible')),
    provider_model_id TEXT NOT NULL,
    hosted BOOLEAN NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('succeeded', 'failed')),
    failure_code TEXT,
    fallback_used BOOLEAN NOT NULL DEFAULT FALSE,
    latency_ms BIGINT NOT NULL CHECK (latency_ms >= 0),
    input_tokens BIGINT CHECK (input_tokens IS NULL OR input_tokens >= 0),
    output_tokens BIGINT CHECK (output_tokens IS NULL OR output_tokens >= 0),
    estimated_cost_usd DOUBLE PRECISION CHECK (
        estimated_cost_usd IS NULL OR estimated_cost_usd >= 0
    ),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX model_inference_runs_created_idx
    ON model_inference_runs (created_at DESC);

CREATE INDEX model_inference_runs_model_idx
    ON model_inference_runs (model_id, created_at DESC);

CREATE INDEX model_inference_runs_hosted_idx
    ON model_inference_runs (hosted, created_at DESC);

-- Consolidated from 20261125100000_model_inference_lan_scope.sql
ALTER TABLE model_inference_runs
    ADD COLUMN trusted_lan BOOLEAN NOT NULL DEFAULT FALSE;

