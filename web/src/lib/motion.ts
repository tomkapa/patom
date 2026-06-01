// Shared motion variants. One source of truth so dropdowns, modals,
// and any future popover use the same easing/duration vocabulary —
// otherwise each consumer drifts slightly and the feel goes muddy.

const EASE_OUT: [number, number, number, number] = [0.2, 0.7, 0.2, 1];

export const popoverMotion = {
  initial: { opacity: 0, scale: 0.96, y: -4 },
  animate: { opacity: 1, scale: 1, y: 0 },
  exit: { opacity: 0, scale: 0.97, y: -2 },
  transition: { duration: 0.18, ease: EASE_OUT },
} as const;

export const modalMotion = {
  initial: { opacity: 0, scale: 0.97 },
  animate: { opacity: 1, scale: 1 },
  exit: { opacity: 0, scale: 0.98 },
  transition: { duration: 0.2, ease: EASE_OUT },
} as const;

export const scrimMotion = {
  initial: { opacity: 0 },
  animate: { opacity: 1 },
  exit: { opacity: 0 },
  transition: { duration: 0.15 },
} as const;

// Off-canvas slide for the mobile Drawer. Left = nav sidebar, right =
// chat thread panel. Same easing as the rest so the feel stays coherent.
export const drawerLeftMotion = {
  initial: { x: "-100%" },
  animate: { x: 0 },
  exit: { x: "-100%" },
  transition: { duration: 0.22, ease: EASE_OUT },
} as const;

export const drawerRightMotion = {
  initial: { x: "100%" },
  animate: { x: 0 },
  exit: { x: "100%" },
  transition: { duration: 0.22, ease: EASE_OUT },
} as const;

// Springy slide for layoutId indicators (sidebar active marker, menu rail
// highlight). Tuned to feel snappy without overshooting hard.
export const indicatorSpring = {
  type: "spring" as const,
  stiffness: 500,
  damping: 35,
};
