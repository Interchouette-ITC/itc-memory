INSERT INTO indexing_runs (id,repository_location_id,root_path,status,configuration) VALUES ($1,$2,$3,'running',$4) RETURNING id
