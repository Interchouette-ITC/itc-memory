UPDATE decisions SET superseded_by_id=$1 WHERE id=$2 AND project_id=$3 AND superseded_by_id IS NULL
