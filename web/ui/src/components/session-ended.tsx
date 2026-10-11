import { Button } from "./button";

/** The screen after a session: what ended, why if it failed, what next. */
export function SessionEnded({
  title,
  reason,
  action,
  onAction,
}: {
  title: string;
  reason?: string;
  action: string;
  onAction: () => void;
}) {
  return (
    <div className="flex h-dvh flex-col items-center justify-center gap-4 bg-background p-6 text-center">
      <h1 className="m-0 text-xl font-semibold tracking-tight">{title}</h1>
      {reason && (
        <p role="alert" className="m-0 max-w-[480px] text-muted-foreground">
          {reason}
        </p>
      )}
      <Button onClick={onAction}>{action}</Button>
    </div>
  );
}
