export function PlaceholderPage({
  title,
  description,
  phase,
}: {
  title: string;
  description: string;
  phase: string;
}) {
  return (
    <>
      <div className="flex flex-col gap-1">
        <h1 className="m-0 text-2xl font-semibold tracking-tight">{title}</h1>
        <span className="text-muted-foreground">{description}</span>
      </div>
      <div className="flex min-h-48 items-center justify-center rounded-[10px] border border-dashed border-border p-8 text-center text-muted-foreground">
        {phase}
      </div>
    </>
  );
}
