import { useId } from "react";

export function Sparkline({ samples, detailed = false }: { samples: readonly number[]; detailed?: boolean }) {
  const id = useId();
  const width = detailed ? 260 : 180;
  const height = detailed ? 78 : 28;
  const peak = Math.max(...samples, 1) * 1.15;
  const values = samples.length < 2 ? [0, samples[0] ?? 0] : samples;
  const points = values.map((value, index) => ({
    x: index * width / (values.length - 1),
    y: (height - 3) * (1 - value / peak),
  }));
  const curve = points.slice(1).reduce((path, point, index) => {
    const previous = points[index];
    const mid = (previous.x + point.x) / 2;
    return path + " C" + mid + " " + previous.y + ", " + mid + " " + point.y + ", " + point.x + " " + point.y;
  }, "M0 " + points[0].y);

  return <svg className={detailed ? "speed-graph" : "spark-svg"} viewBox={`0 0 ${width} ${height}`}
    preserveAspectRatio="none" aria-hidden="true">
    <defs><linearGradient id={id} x2="0" y2="1">
      <stop stopColor="var(--accent)" stopOpacity={detailed ? .24 : .22} />
      <stop offset="100%" stopColor="var(--accent)" stopOpacity="0" />
    </linearGradient></defs>
    {detailed && [20, 48].map(y => <line key={y} x1="0" y1={y} x2={width} y2={y}
      stroke="var(--border)" strokeDasharray="3 3" />)}
    <path d={`${curve} L${width} ${height} H0 Z`} fill={`url(#${id})`} />
    <path d={curve} fill="none" stroke="var(--accent)" strokeWidth={detailed ? 2 : 1.7} strokeLinecap="round" />
  </svg>;
}
