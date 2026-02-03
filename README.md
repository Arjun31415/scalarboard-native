# ScalarBoard
A high-performance TensorBoard-like viewer in Rust for scalars

Much faster than the web based tensorboard viewer because multithreading and native UI. (btw tensorboard backend uses rust not python :) )

# Why - 

tensorboard web viewer slow when I had around 10Mil data points, So I make this. And I wanted binning of data dont want to see the noisy plot, and tensorboard smoothing does not have the filled area graph which shows std deviation

# Features - 
1. Blazingly Fast ( sorry I wanted to say it once)
2. dynamic binning
3. Automatically separates binning logic for "eval" tag vs "train" tag. Eval tags are never binned (assumption is that eval tags have much less volume so there is no point in binning and you want to see every data point for eval as compared to train. Eval tags are those which start with tag name "eval/")
4. Thumbnail plots and full screen plots 
5. Legends, interactive plot area (feature of egui-plot)
6. Dark and Light mode
7. Save a plot to png


More features may be added in future if I feel the need for it, not sure about adding plotting stuff for non-scalars like images etc (primarily because I dont use that, but open to it if I need it in the future)


<img width="1920" height="1080" alt="plot_export" src="https://github.com/user-attachments/assets/e844edf9-4bab-4d25-bae6-eba4683caf70" />

<img width="1920" height="1032" alt="plot_export" src="https://github.com/user-attachments/assets/a481971f-d511-410b-bfff-3de077849a92" />



# How to use - 

```sh
git clone 
cd 
cargo run --release -- --path=<path/to/tfevents/directory>
# Or
# cargo build --release && ./target/release/scalarboard-native --path=<path/to/tfevents/directory> 

```


