import { type Task, type TaskStatus } from "./todo.js";
type Follow = {
    id: string;
    after: number;
    on: TaskStatus[];
};
type Acknowledgment = {
    id: string;
    revision: number;
};
export declare function followPath(fan: string, ledger?: string): string;
export declare function readFollowing(fan: string, ledger?: string): Follow[];
export declare function followTask(fan: string, id: string, on?: TaskStatus[]): Promise<{
    following: Follow[];
}>;
export declare function acknowledgeFeed(fan: string, acks: Acknowledgment[]): Promise<void>;
export declare function makeFeed(following: Follow[], tasks: Task[]): {
    events: {
        revision: number;
        from: TaskStatus;
        to: TaskStatus;
        at: string;
        actor?: string;
        id: string;
        title: string;
        workspaceId: string | undefined;
        product: string | undefined;
    }[];
    total: number;
    more: boolean;
    gaps: {
        id: string;
        prunedThrough: number;
    }[];
    latest: Record<string, number>;
};
export declare function readFeed(fan: string, ledger?: string): {
    events: {
        revision: number;
        from: TaskStatus;
        to: TaskStatus;
        at: string;
        actor?: string;
        id: string;
        title: string;
        workspaceId: string | undefined;
        product: string | undefined;
    }[];
    total: number;
    more: boolean;
    gaps: {
        id: string;
        prunedThrough: number;
    }[];
    latest: Record<string, number>;
};
export declare const readFeedTasks: (ledger: string) => Task[];
export declare function watchTodoChanges(ledger: string, refresh: (file: string | null) => void, failed?: () => void): () => void;
export {};
