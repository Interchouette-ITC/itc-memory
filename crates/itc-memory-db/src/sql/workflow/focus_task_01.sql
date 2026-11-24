UPDATE projects p SET current_task_id=$2,updated_at=now() WHERE p.slug=$1 AND EXISTS(SELECT 1 FROM tasks t WHERE t.id=$2 AND t.project_id=p.id)
