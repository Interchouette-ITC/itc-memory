INSERT INTO commits (id,repository_id,sha,subject) VALUES ($1,$2,$3,$4) ON CONFLICT (repository_id,sha) DO NOTHING
