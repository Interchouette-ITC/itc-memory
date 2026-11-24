SELECT id FROM repositories WHERE project_id=$1 AND (display_name=$2 OR canonical_remote=$2) ORDER BY display_name LIMIT 1
