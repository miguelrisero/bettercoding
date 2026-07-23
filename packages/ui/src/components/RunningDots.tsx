export function RunningDots() {
  return (
    <div className="flex items-center gap-[2px] shrink-0">
      <span className="size-dot rounded-full bg-brand animate-running-dot-1 motion-reduce:animate-none" />
      <span className="size-dot rounded-full bg-brand animate-running-dot-2 motion-reduce:animate-none" />
      <span className="size-dot rounded-full bg-brand animate-running-dot-3 motion-reduce:animate-none" />
    </div>
  );
}
