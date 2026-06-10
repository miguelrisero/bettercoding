interface BrandLogoProps {
  className?: string;
}

export function BrandLogo({ className }: BrandLogoProps) {
  return (
    <span
      className={`font-ibm-plex-mono text-xl font-semibold tracking-tight text-high select-none ${className ?? ""}`}
    >
      Better<span className="text-brand">Coding</span>
    </span>
  );
}
