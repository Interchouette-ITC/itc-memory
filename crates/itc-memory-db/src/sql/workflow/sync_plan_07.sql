UPDATE tasks SET phase_id=$2,title=$3,status=CASE WHEN status_source='plan' THEN $4 ELSE status END,checkbox_checked=$5,source_reference=$6,updated_at=now() WHERE id=$1
