//! `String.split` with Java's trailing-empty-field rule: a run of empty
//! fields at the end of a line is dropped, and a surplus field is not.

pub(crate) struct JavaSplitFields<'line> {
    pub(crate) first_fields: [Option<&'line str>; 7],
    pub(crate) length: usize,
}

impl<'line> JavaSplitFields<'line> {
    pub(crate) fn get(&self, index: usize) -> Option<&'line str> {
        if index < self.length {
            self.first_fields.get(index).copied().flatten()
        } else {
            None
        }
    }
}

pub(crate) fn split_like_java(line: &str) -> JavaSplitFields<'_> {
    let mut first_fields = [None; 7];
    let mut split_field_count = 0usize;
    let mut last_nonempty_field_count = 0usize;
    for field in line.split(',') {
        if split_field_count < first_fields.len() {
            first_fields[split_field_count] = Some(field);
        }
        split_field_count = split_field_count.saturating_add(1);
        if !field.is_empty() {
            last_nonempty_field_count = split_field_count;
        }
    }
    JavaSplitFields {
        first_fields,
        length: if line.is_empty() {
            1
        } else {
            last_nonempty_field_count
        },
    }
}
