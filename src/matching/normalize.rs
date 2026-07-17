use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Normalizes text for matching while retaining words such as "live" or
/// "remix", which carry meaningful version information.
pub fn normalize(value: &str) -> String {
    let punctuation_normalized = value
        .replace(['’', '‘', '`', '´'], "'")
        .replace(['–', '—', '―'], "-")
        .replace('…', "...");

    let without_accents: String = punctuation_normalized
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .collect();

    let mut words = Vec::new();
    let mut current = String::new();

    for character in without_accents.to_lowercase().chars() {
        if character.is_alphanumeric() {
            current.push(character);
        } else {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }

            if matches!(character, '&') {
                words.push("and".to_owned());
            }
        }
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
        .into_iter()
        .map(|word| match word.as_str() {
            "featuring" | "ft" => "feat".to_owned(),
            _ => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn normalize_artist(value: &str) -> String {
    normalize(value)
}

#[cfg(test)]
mod tests {
    use super::{normalize, normalize_artist};

    #[test]
    fn normalizes_spanish_punctuation_and_accents() {
        assert_eq!(normalize("¿Para Qué Me Hablas?"), "para que me hablas");
        assert_eq!(normalize("Para Que Me Hablas"), "para que me hablas");
        assert_eq!(
            normalize("  Jueves   en el Colectivo "),
            "jueves en el colectivo"
        );
    }

    #[test]
    fn normalizes_feature_artist_markers() {
        assert_eq!(normalize("Canción feat. Artista"), "cancion feat artista");
        assert_eq!(normalize("Canción ft. Artista"), "cancion feat artista");
        assert_eq!(
            normalize("Canción featuring Artista"),
            "cancion feat artista"
        );
    }

    #[test]
    fn normalizes_artist_names() {
        assert_eq!(normalize_artist("  Willie   Colón "), "willie colon");
    }

    #[test]
    fn preserves_version_words() {
        assert_eq!(normalize("Canción (En Vivo)"), "cancion en vivo");
        assert_eq!(
            normalize("Canción - Remastered 2024"),
            "cancion remastered 2024"
        );
    }
}
