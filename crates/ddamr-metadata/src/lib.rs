//! ddamr-metadata: lectura/escritura EXIF/XMP/IPTC (fase 1-2).
//!
//! Hot path: lectura nativa. ExifTool solo en lote, para escritura
//! y para el volcado raw, siempre por argv (nunca por shell) y
//! siempre sobre copia. Stub en fase 0.
