export type StatsRange = "1h" | "24h" | "7d" | "30d" | "90d" | "all";
export type StatsStyle = "default" | "modern";
export type StatsTheme = "system" | "light" | "dark";
export type StatsPage =
  | "overview"
  | "models"
  | "providers"
  | "costs"
  | "requests"
  | "errors"
  | "traces"
  | "tools"
  | "hooks"
  | "projects"
  | "frustration"
  | "gain";

export interface CostSnapshot {
  input: number;
  cache_read: number;
  cache_write: number;
  output: number;
}

export interface RateSnapshot {
  input: number;
  cached_input: number;
  cache_write: number | null;
  output: number;
}

export interface RequestRow {
  id: number;
  request_key: string;
  timestamp: number;
  model: string;
  provider: string;
  model_reference: string;
  api: string;
  project: string;
  cwd: string;
  purpose: string;
  session_file: string | null;
  entry_id: string | null;
  duration_ms: number;
  ttft_ms: number | null;
  input_tokens: number | null;
  output_tokens: number | null;
  cached_tokens: number | null;
  cache_write_tokens: number | null;
  status: string;
  stop_reason: string | null;
  error_message: string | null;
  cost: number | null;
  cost_components: CostSnapshot | null;
  has_context?: boolean;
}

export interface SkillHeader {
  name: string;
  description: string;
  uri: string;
}

export interface RequestContextSnapshot {
  request: Record<string, unknown> & {
    instructions?: string;
    messages?: unknown;
    input?: unknown;
    tools?: unknown[];
  };
  skills: SkillHeader[];
}

export interface RequestDetailResponse {
  request: RequestRow;
  request_context: RequestContextSnapshot | null;
}

export interface AggregateTotals {
  requests: number;
  ok: number;
  failed: number;
  aborted: number;
  input_tokens: number | null;
  output_tokens: number | null;
  cached_tokens: number | null;
  uncached_tokens: number | null;
  tokens: number | null;
  usage_known: number;
  cache_known: number;
  cache_write_known: number;
  cache_write_tokens: number | null;
  avg_duration_ms: number | null;
  avg_ttft_ms: number | null;
  tokens_per_second: number | null;
  first_seen: number | null;
  last_seen: number | null;
  priced: number;
  unpriced: number;
  cost: number | null;
  cost_components: CostSnapshot | null;
}

export interface AggregateBucket extends AggregateTotals {
  timestamp: number;
}

export interface AggregateHour extends AggregateTotals {
  hour: number;
}

export interface ModelAggregate extends AggregateTotals {
  key: string;
  model: string;
  provider: string;
  rates: RateSnapshot | null;
  buckets: AggregateBucket[];
  daily: AggregateBucket[];
  hours: AggregateHour[];
  models: number;
}

export interface ProviderAggregate extends AggregateTotals {
  key: string;
  provider: string;
  buckets: AggregateBucket[];
  daily: AggregateBucket[];
  hours: AggregateHour[];
  models: number;
}

export interface AnalyticsResponse {
  summary: AggregateTotals;
  buckets: AggregateBucket[];
  daily: AggregateBucket[];
  hours: AggregateHour[];
  models: ModelAggregate[];
  providers: ProviderAggregate[];
  active_days: number;
  range: StatsRange;
  updated_at: number;
  latest?: RequestRow[];
}

export interface RequestsResponse {
  requests: RequestRow[];
  has_more: boolean;
  limit: number;
  updated_at: number;
  range: StatsRange;
}

export interface ErrorsResponse {
  requests: RequestRow[];
  has_more: boolean;
  limit: number;
  range: StatsRange;
  updated_at: number;
}

export interface TraceSessionSummary extends AggregateTotals {
  id: number;
  title?: string;
  session_file: string | null;
  project: string;
  cwd: string;
  tool_calls: number;
  agents: number;
  wall_ms: number;
  model_ms: number;
  models: Array<{ key: string; model: string; provider: string }>;
}

export interface TracesListResponse {
  sessions: TraceSessionSummary[];
  total: number;
  has_more: boolean;
  limit: number;
  range: StatsRange;
  updated_at: number;
}

export interface TraceToolCall {
  id: number;
  request_key: string | null;
  session_file: string;
  call_id: string;
  tool: string;
  timestamp: number;
  duration_ms: number;
  status: string;
  error_message: string | null;
  request_id: number | null;
}

export interface TraceToolStat {
  tool: string;
  calls: number;
  errors: number;
  total_ms: number;
  max_ms: number;
}

export interface TraceDetailSummary extends AggregateTotals {
  wall_ms: number;
  model_ms: number;
  tool_ms: number;
  tool_calls: number;
  idle_ms: number;
  agents: number;
  models: Array<{ key: string; model: string; provider: string }>;
}

export interface TraceDetailResponse {
  id: number;
  session_file: string | null;
  project: string;
  cwd: string;
  title?: string;
  summary: TraceDetailSummary;
  requests: RequestRow[];
  tool_calls: TraceToolCall[];
  tool_stats: TraceToolStat[];
  updated_at: number;
}

export interface ToolMetricRow {
  calls: number;
  args_chars: number;
  tokens: number | null;
  output_tokens: number | null;
  usage_known: number;
  cost: number | null;
  priced: number;
  unpriced: number;
  last_seen: number;
  errors: number | null;
  cancelled: number | null;
  executions: number | null;
  result_chars: number | null;
  result_calls: number;
  avg_duration_ms: number | null;
  buckets: Array<{ timestamp: number; requests: number }>;
  error_buckets?: Array<{ timestamp: number; errors: number }>;
  recent: Array<{
    id: number;
    model: string;
    provider: string;
    timestamp: number;
    status: string;
    stop_reason: string | null;
  }>;
}

export interface ToolModelRow extends ToolMetricRow {
  key: string;
  model: string;
  provider: string;
}

export interface ToolRow extends ToolMetricRow {
  key: string;
  tool: string;
  models: ToolModelRow[];
}

export interface ToolsResponse {
  summary: ToolMetricRow;
  tools: ToolRow[];
  requests_with_tools: number;
  partial_requests: number;
  updated_at: number;
  range: StatsRange;
}

export interface HookCounters {
  runs: number;
  ok: number;
  blocked: number;
  errors: number;
  modified: number;
  total_duration_ms: number;
  avg_duration_ms: number | null;
  max_duration_ms: number | null;
  first_seen: number | null;
  last_seen: number | null;
}

export interface HookEventRow extends HookCounters {
  event: string;
  sessions: number;
  tools: Array<HookCounters & { tool: string }>;
}

export interface HookProjectRow extends HookCounters {
  key: string;
  project: string;
  sessions: number;
  events: number;
}

export interface HooksResponse {
  summary: HookCounters & { sessions: number };
  buckets: Array<HookCounters & { timestamp: number }>;
  events: HookEventRow[];
  projects: HookProjectRow[];
  updated_at: number;
  range: StatsRange;
}

export interface ProjectRow extends AggregateTotals {
  key: string;
  project: string;
  sessions: number;
  requests_without_session: number;
  recent: RequestRow[];
  buckets: AggregateBucket[];
  models: Array<
    AggregateTotals & {
      key: string;
      model: string;
      provider: string;
    }
  >;
}

export interface ProjectsResponse {
  summary: AggregateTotals;
  projects: ProjectRow[];
  updated_at: number;
  range: StatsRange;
}

export interface StyleSettingsResponse {
  style: StatsStyle;
}

export type SortDirection = "asc" | "desc" | "";

export interface SortState {
  key: string;
  dir: SortDirection;
}
