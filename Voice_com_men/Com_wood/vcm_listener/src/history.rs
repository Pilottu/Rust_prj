#![allow(dead_code)]

// История последних секунд звука (16 кГц, моно) с глобальной нумерацией отсчётов.
// Детектор речи начинает фразу с небольшим опозданием и отдаёт сегмент без звука
// перед ней. Здесь мы находим сегмент в истории и берём реальный звук до и после него.

pub struct History {
    buf: Vec<f32>,
    /// Глобальный номер отсчёта, лежащего в buf[0].
    base: usize,
    capacity: usize,
}

impl History {
    /// Хранится не менее `capacity` последних отсчётов.
    pub fn new(capacity: usize) -> Self {
        Self { buf: Vec::new(), base: 0, capacity }
    }

    pub fn push(&mut self, chunk: &[f32]) {
        self.buf.extend_from_slice(chunk);
        // Обрезаем не на каждом куске, а когда накопилось вдвое больше (дёшево по памяти).
        if self.buf.len() > self.capacity * 2 {
            let drop = self.buf.len() - self.capacity;
            self.buf.drain(..drop);
            self.base += drop;
        }
    }

    /// Глобальный номер отсчёта сразу за последним сохранённым.
    pub fn end(&self) -> usize {
        self.base + self.buf.len()
    }

    /// Где в потоке лежит `needle`: точное совпадение первых отсчётов
    /// (детектор речи отдаёт сегмент без изменений).
    pub fn find(&self, needle: &[f32]) -> Option<usize> {
        let n = needle.len().min(512);
        if n == 0 || self.buf.len() < n {
            return None;
        }
        let needle = &needle[..n];
        let first = needle[0].to_bits();
        for i in 0..=(self.buf.len() - n) {
            if self.buf[i].to_bits() == first
                && self.buf[i..i + n]
                    .iter()
                    .zip(needle)
                    .all(|(a, b)| a.to_bits() == b.to_bits())
            {
                return Some(self.base + i);
            }
        }
        None
    }

    /// Отсчёты [from, to) из истории; границы подрезаются по тому, что сохранено.
    /// Возвращает фактическое начало и сами отсчёты.
    pub fn range(&self, from: usize, to: usize) -> (usize, &[f32]) {
        let from = from.max(self.base);
        let to = to.min(self.end());
        if from >= to {
            return (from, &[]);
        }
        (from, &self.buf[from - self.base..to - self.base])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(from: usize, n: usize) -> Vec<f32> {
        (from..from + n).map(|i| (i as f32) * 0.001 + 0.5).collect()
    }

    #[test]
    fn finds_exact_segment_and_reads_context_around_it() {
        let mut h = History::new(10_000);
        let all = ramp(0, 5_000);
        for chunk in all.chunks(160) {
            h.push(chunk);
        }
        let segment = &all[2_000..3_000];
        let start = h.find(segment).expect("сегмент должен найтись");
        assert_eq!(start, 2_000);

        let (from, ctx) = h.range(start - 300, start + segment.len() + 200);
        assert_eq!(from, 1_700);
        assert_eq!(ctx.len(), 300 + 1_000 + 200);
        assert_eq!(&ctx[300..1_300], segment);
    }

    #[test]
    fn numbering_survives_trimming() {
        let mut h = History::new(1_000);
        let all = ramp(0, 10_000);
        for chunk in all.chunks(100) {
            h.push(chunk);
        }
        // старое уже выброшено, свежее находится под прежним глобальным номером
        assert!(h.find(&all[100..200]).is_none());
        let start = h.find(&all[9_000..9_500]).unwrap();
        assert_eq!(start, 9_000);
        assert_eq!(h.end(), 10_000);
    }

    #[test]
    fn range_is_clipped_to_what_is_stored() {
        let mut h = History::new(1_000);
        let all = ramp(0, 600);
        h.push(&all);
        // до начала истории и после её конца — подрезаем
        let (from, ctx) = h.range(0, 10_000);
        assert_eq!(from, 0);
        assert_eq!(ctx.len(), 600);
        let (_, empty) = h.range(700, 800);
        assert!(empty.is_empty());
    }

    #[test]
    fn missing_or_empty_needle_is_none() {
        let mut h = History::new(1_000);
        h.push(&ramp(0, 300));
        assert!(h.find(&[]).is_none());
        assert!(h.find(&ramp(5_000, 50)).is_none());
        let mut tiny = History::new(10);
        tiny.push(&[0.1, 0.2]);
        assert!(tiny.find(&ramp(0, 50)).is_none()); // иголка длиннее истории
    }

    #[test]
    fn pre_roll_is_shorter_when_phrase_starts_near_beginning() {
        let mut h = History::new(10_000);
        let all = ramp(0, 2_000);
        h.push(&all);
        let start = h.find(&all[100..400]).unwrap();
        let (from, _) = h.range(start.saturating_sub(4_800), start + 300);
        assert_eq!(from, 0); // 0.3 с до фразы взять неоткуда — берём сколько есть
    }
}
