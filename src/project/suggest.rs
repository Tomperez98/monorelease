pub(super) fn closest_name<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a String>,
) -> Option<String> {
    let max_distance = if name.len() <= 4 { 1 } else { 2 };
    candidates
        .into_iter()
        .filter(|candidate| candidate.as_str() != name)
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= max_distance)
        .min_by(|(left_distance, left), (right_distance, right)| {
            left_distance
                .cmp(right_distance)
                .then_with(|| left.cmp(right))
        })
        .map(|(_, candidate)| candidate.clone())
}

pub(super) fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_byte) in left.bytes().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_byte) in right.bytes().enumerate() {
            let substitution = previous[right_index] + usize::from(left_byte != right_byte);
            let insertion = current[right_index] + 1;
            let deletion = previous[right_index + 1] + 1;
            current.push(substitution.min(insertion).min(deletion));
        }
        previous = current;
    }
    previous[right.len()]
}
