import { formatDeviceId } from "../lib/utils";
import { Input, Label } from "./input";

/** The PIN a host shows for pairing: six digits, anything else dropped. */
export function PinField({
  id,
  value,
  onChange,
}: {
  id: string;
  value: string;
  onChange: (pin: string) => void;
}) {
  return (
    <div className="flex flex-col gap-2">
      <Label htmlFor={id}>PIN</Label>
      <Input
        id={id}
        inputMode="numeric"
        autoComplete="one-time-code"
        placeholder="000000"
        value={value}
        onChange={(e) => onChange(e.target.value.replace(/\D/g, "").slice(0, 6))}
        className="h-11 font-mono text-base tracking-[0.3em]"
      />
    </div>
  );
}

/** A device's ID as it is typed: nine digits in groups of three. */
export function DeviceIdField({
  id,
  value,
  onChange,
}: {
  id: string;
  value: string;
  onChange: (id: string) => void;
}) {
  return (
    <div className="flex flex-col gap-2">
      <Label htmlFor={id}>Geräte-ID</Label>
      <Input
        id={id}
        inputMode="numeric"
        autoComplete="off"
        placeholder="000 000 000"
        value={value}
        onChange={(e) => onChange(formatDeviceId(e.target.value).slice(0, 11))}
        className="h-11 font-mono text-base tracking-wider"
      />
    </div>
  );
}
