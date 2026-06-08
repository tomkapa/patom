import {
  BookOpen,
  Briefcase,
  CalendarClock,
  Calculator,
  Crown,
  Inbox,
  LifeBuoy,
  Megaphone,
  PenLine,
  PencilRuler,
  PartyPopper,
  Search,
  Send,
  TrendingUp,
  UserRoundSearch,
  type LucideIcon,
} from "lucide-react";

/** Lookup table from the `icon` strings used in `teamPresets.ts` (and
 *  similar data files) to the actual lucide-react component. Adding a
 *  new icon to a preset requires adding it here too — TypeScript will
 *  surface unmapped names at the call site via the `name` prop type. */
const ICONS = {
  "book-open": BookOpen,
  briefcase: Briefcase,
  "calendar-clock": CalendarClock,
  calculator: Calculator,
  crown: Crown,
  inbox: Inbox,
  "life-buoy": LifeBuoy,
  megaphone: Megaphone,
  "pen-line": PenLine,
  "pencil-ruler": PencilRuler,
  "party-popper": PartyPopper,
  search: Search,
  send: Send,
  "trending-up": TrendingUp,
  "user-round-search": UserRoundSearch,
} as const satisfies Record<string, LucideIcon>;

export type LucideName = keyof typeof ICONS;

/** Resolve an icon by string name. Pass `name` as a string from data
 *  files; if it doesn't match a known icon, we render a neutral square
 *  so the layout doesn't break in dev. */
export function LucideByName({
  name,
  size = 16,
  className,
}: {
  name: string;
  size?: number;
  className?: string;
}) {
  const C = (ICONS as Record<string, LucideIcon | undefined>)[name];
  if (!C) {
    // Fallback: render an empty slot so the layout doesn't break in dev
    // if a preset references an icon we haven't mapped yet.
    return (
      <span
        aria-hidden="true"
        style={{ width: size, height: size, display: "inline-block" }}
      />
    );
  }
  return <C size={size} className={className} />;
}
