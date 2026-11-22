// What goes from the window when DBine Enterprise's access ends (the backend
// deletes its own stores; this is the part that lives in the UI).

/** A DBine Enterprise (managed) connection's id. */
export const isCompanyId = (id: string | null | undefined): boolean => !!id && id.startsWith('ent-');

interface Tagged { company?: boolean }

/** Whether a conversation used a company connection. */
export const usedCompany = (messages: Tagged[]): boolean => messages.some((m) => m.company);

/** The archive without the conversations that used a company connection. */
export function withoutCompanyChats<T extends { messages: Tagged[] }>(list: T[]): T[] {
  return list.filter((c) => !usedCompany(c.messages));
}
