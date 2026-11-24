SELECT id FROM plans WHERE project_id=$1 AND is_current LIMIT 1
