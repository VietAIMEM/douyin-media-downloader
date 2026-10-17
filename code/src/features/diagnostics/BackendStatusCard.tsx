import { Badge, Card, KeyValue, KeyValueList } from "@/components/ui";
import { formatDateTime, humanize } from "@/lib/format";
import type { AppInfo, DatabaseStatus } from "@/types";

interface BackendStatusCardProps {
  appInfo: AppInfo | null;
  databaseStatus: DatabaseStatus | null;
}

/**
 * Read-only diagnostics for the running backend.
 *
 * Lives on the Settings screen: it answers "what is this build, where does it
 * write, and is the database healthy" — the questions of the moment something
 * seems wrong. Everything here comes from real commands (`get_app_info`,
 * `get_database_status`); the values are what the process actually resolved at
 * startup, which is what makes it useful for debugging a bad install.
 */
export function BackendStatusCard({ appInfo, databaseStatus }: BackendStatusCardProps) {
  return (
    <div className="space-y-5">
      <h2 className="text-sm font-semibold text-content-primary">Application information</h2>
      <div className="grid gap-5 lg:grid-cols-2">
      <Card title="Application" description="Build identity and resolved directories">
        {appInfo ? (
          <KeyValueList>
            <KeyValue label="Name">{appInfo.name}</KeyValue>
            <KeyValue label="Version" mono>
              {appInfo.version}
            </KeyValue>
            <KeyValue label="Identifier" mono>
              {appInfo.identifier}
            </KeyValue>
            <KeyValue label="Platform" mono>
              {appInfo.os}/{appInfo.architecture}
            </KeyValue>
            <KeyValue label="Started" mono>
              {formatDateTime(appInfo.startedAt)}
            </KeyValue>
            <KeyValue label="Data directory" mono>
              {appInfo.dataDirectory}
            </KeyValue>
            <KeyValue label="Log directory" mono>
              {appInfo.logDirectory}
            </KeyValue>
            <KeyValue label="Log level" mono>
              {appInfo.logLevel}
            </KeyValue>
          </KeyValueList>
        ) : (
          <p className="text-xs text-content-muted">Waiting for the backend…</p>
        )}
      </Card>

      <Card
        title="Database"
        description="SQLite location, schema version, and row counts"
        action={
          databaseStatus && (
            <Badge tone={databaseStatus.healthy ? "success" : "danger"}>
              {databaseStatus.healthy ? "healthy" : "unavailable"}
            </Badge>
          )
        }
      >
        {databaseStatus ? (
          <div className="space-y-5">
            <KeyValueList>
              <KeyValue label="File" mono>
                {databaseStatus.location}
              </KeyValue>
              <KeyValue label="Persistent">
                {databaseStatus.isPersistent ? "yes" : "no (in-memory)"}
              </KeyValue>
              <KeyValue label="Schema" mono>
                v{databaseStatus.schemaVersion} of v{databaseStatus.latestSchemaVersion}
              </KeyValue>
              <KeyValue label="Journal mode" mono>
                {databaseStatus.journalMode}
              </KeyValue>
              <KeyValue label="Foreign keys">
                {databaseStatus.foreignKeys ? "enforced" : "off"}
              </KeyValue>
            </KeyValueList>

            <div>
              <p className="mb-2 text-xs font-medium text-content-primary">Tables</p>
              <ul className="flex flex-wrap gap-2">
                {databaseStatus.tableCounts.map((entry) => (
                  <li key={entry.table}>
                    <Badge mono>
                      {entry.table} · {entry.rows}
                    </Badge>
                  </li>
                ))}
              </ul>
            </div>

            <div>
              <p className="mb-2 text-xs font-medium text-content-primary">Migrations applied</p>
              <ul className="space-y-1.5">
                {databaseStatus.appliedMigrations.map((migration) => (
                  <li key={migration.version} className="flex items-baseline gap-2 text-xs">
                    <span className="font-mono text-content-muted">
                      {String(migration.version).padStart(4, "0")}
                    </span>
                    <span className="text-content-primary">{migration.description}</span>
                    <span className="ml-auto shrink-0 text-content-muted">
                      {formatDateTime(migration.appliedAt)}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          </div>
        ) : (
          <p className="text-xs text-content-muted">Waiting for the backend…</p>
        )}
      </Card>

      <Card
        className="lg:col-span-2"
        title="Content sources"
        description="The components registered to read Douyin at startup"
      >
        {appInfo ? (
          <ul className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
            {appInfo.resolvers.map((resolver) => (
              <li
                key={`${resolver.name}-${resolver.capability}`}
                className="rounded-lg border border-surface-border px-3 py-2.5"
              >
                <p className="truncate font-mono text-xs text-content-primary">{resolver.name}</p>
                <p className="mt-1 text-xs text-content-muted">{humanize(resolver.capability)}</p>
                <p className="mt-2">
                  <Badge mono tone={resolver.platform === "douyin" ? "brand" : "neutral"}>
                    {resolver.platform}
                  </Badge>
                </p>
              </li>
            ))}
          </ul>
        ) : (
          <p className="text-xs text-content-muted">Waiting for the backend…</p>
        )}
      </Card>
      </div>
    </div>
  );
}
