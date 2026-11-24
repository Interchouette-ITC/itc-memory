INSERT INTO decisions (id,project_id,source_document_id,source_revision_id,title,rationale,status,last_verified_at) VALUES ($1,$2,$3,$4,$5,$6,'validated',now())
