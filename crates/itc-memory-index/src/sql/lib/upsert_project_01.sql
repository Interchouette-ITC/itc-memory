INSERT INTO projects (id, slug, display_name) VALUES ($1,$2,$3) ON CONFLICT (slug) DO UPDATE SET display_name=EXCLUDED.display_name, updated_at=now() RETURNING id
