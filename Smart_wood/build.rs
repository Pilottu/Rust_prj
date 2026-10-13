fn main() {
    // Говорим компилятору собрать наш файл интерфейса
    slint_build::compile("ui/app.slint").unwrap();
}
