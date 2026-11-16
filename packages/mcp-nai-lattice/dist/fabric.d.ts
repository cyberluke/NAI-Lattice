/** Filter a record (payload flat shape). */
interface Rec {
    runtime_sequence?: number;
    timestamp_utc_ms?: number;
    ts_ms?: number;
    level?: string;
    component?: string;
    trace_id?: string;
    caller?: string;
    message?: string;
    extras?: Record<string, unknown>;
}
export interface QueryOpts {
    level?: string;
    component?: string;
    trace?: string;
    contains?: string;
    limit: number;
}
/** Open a short-lived fabric client and fetch the bounded tail of log records. */
export declare function fetchLogs(opts: QueryOpts): Promise<{
    records: Rec[];
    error?: string;
}>;
/** Aggregate traffic/health stats from a bounded recent scan. */
export declare function fetchStats(): Promise<{
    window: number;
    levels: Record<string, number>;
    errors: number;
    components: Record<string, number>;
    distinctTraceIds: number;
    error?: string;
}>;
export {};
