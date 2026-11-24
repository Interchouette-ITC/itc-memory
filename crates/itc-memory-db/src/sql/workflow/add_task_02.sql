SELECT ph.id FROM phases ph JOIN plans pl ON pl.id=ph.plan_id WHERE pl.project_id=$1 AND pl.is_current AND ph.ordinal=$2
