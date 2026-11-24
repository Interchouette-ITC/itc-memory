UPDATE sessions SET ended_at=now(),summary=COALESCE($2,summary) WHERE id=$1 AND ended_at IS NULL
