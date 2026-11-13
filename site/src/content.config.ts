import { defineCollection, z } from "astro:content";
import { docsLoader } from "@astrojs/starlight/loaders";
import { docsSchema } from "@astrojs/starlight/schema";

export const collections = {
  docs: defineCollection({
    loader: docsLoader(),
    schema: docsSchema({
      extend: z.object({
        category: z.enum(["setup", "tools", "evidence", "architecture"]).optional(),
        verified: z.coerce.date().optional(),
      }),
    }),
  }),
};
