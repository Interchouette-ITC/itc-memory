INSERT INTO repositories (id,project_id,canonical_remote,display_name,last_verified_at) VALUES ($1,$2,$3,$4,now()) ON CONFLICT (canonical_remote) DO UPDATE SET last_verified_at=now() RETURNING id
