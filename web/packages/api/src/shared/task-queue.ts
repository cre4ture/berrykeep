export type TaskQueueState = "idle" | "waiting" | "running" | "backlogged" | "unavailable";

export type TaskQueueEntry = {
  id: string;
  label: string;
  pending: number;
  active: number;
  capacity?: number | null;
  state: TaskQueueState;
  detail?: string | null;
};

export type ClusterTaskQueueNodeSnapshot = {
  node_id: string;
  queues: TaskQueueEntry[];
};

export type ClusterTaskQueueSnapshot = {
  generated_at_unix_ms: number;
  nodes: ClusterTaskQueueNodeSnapshot[];
  unavailable_nodes: Array<{
    node_id: string;
    error: string;
  }>;
};
