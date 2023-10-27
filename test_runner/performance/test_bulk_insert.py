from contextlib import closing

from fixtures.compare_fixtures import NeonCompare, PgCompare
from fixtures.pageserver.utils import wait_tenant_status_404
from fixtures.pg_version import PgVersion


#
# Run bulk INSERT test.
#
# Collects metrics:
#
# 1. Time to INSERT 5 million rows
# 2. Disk writes
# 3. Disk space used
# 4. Peak memory usage
#
def test_bulk_insert(neon_with_baseline: PgCompare):
    env = neon_with_baseline

    with closing(env.pg.connect()) as conn:
        with conn.cursor() as cur:
            cur.execute("create table huge (i int, j int);")

            # Run INSERT, recording the time and I/O it takes
            with env.record_pageserver_writes("pageserver_writes"):
                with env.record_duration("insert"):
                    cur.execute("insert into huge values (generate_series(1, 5000000), 0);")
                    env.flush()

            env.report_peak_memory_use()
            env.report_size()

    # When testing neon, also check how long it takes the pageserver to reingest the
    # wal from safekeepers. If this number is close to total runtime, then the pageserver
    # is the bottleneck.
    if isinstance(env, NeonCompare):
        measure_recovery_time(env)


def measure_recovery_time(env: NeonCompare):
    client = env.env.pageserver.http_client()
    pg_version = PgVersion(client.timeline_detail(env.tenant, env.timeline)["pg_version"])

    # Delete the Tenant in the pageserver: this will drop local and remote layers, such that
    # when we "create" the Tenant again, we will replay the WAL from the beginning.
    client.tenant_delete(env.tenant)
    wait_tenant_status_404(client, env.tenant, iterations=60, interval=0.5)
    client.tenant_create(new_tenant_id=env.tenant)

    # Measure recovery time
    with env.record_duration("wal_recovery"):
        client.timeline_create(pg_version, env.tenant, env.timeline)

        # Flush, which will also wait for lsn to catch up
        env.flush()
