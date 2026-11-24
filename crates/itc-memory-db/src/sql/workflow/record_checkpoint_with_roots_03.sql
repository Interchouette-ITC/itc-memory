SELECT EXISTS(SELECT 1 FROM sessions WHERE id=$1 AND project_id=$2)
