//! Tests for texture copy

use wgpu_test::{initialize_test, TestParameters};

use wasm_bindgen_test::*;


fn total_bytes_in_copy(   
    texture_format:     wgpu::TextureFormat, 
    bytes_per_row:u32,
    rows_per_image: u32 ,
    depth_or_array_layers: u32 , 
 ) -> u32  {

let  block_size = texture_format.block_size(None).unwrap_or(1) ; 

println!("block_size  : {:?}" , texture_format.block_size(None));
println!("real block_size  : {:?}" , block_size);
let block_dim = texture_format.block_dimensions();



let bytes_per_image = bytes_per_row * rows_per_image;
let mut total_bytes = bytes_per_image*depth_or_array_layers;


if block_dim.1 != 0 {
    let last_row_bytes = block_dim.0  * block_size;
let  last_image_bytes = bytes_per_row * (block_dim.1 - 1) + last_row_bytes;
total_bytes += last_image_bytes;
}


total_bytes
}


fn test(format :wgpu::TextureFormat ,w: u32 , h:u32 ,bytes_per_row:u32 , rows_per_image:u32  , total_size_in_bytes:u32   , mip_level:u32 ,  mips_count: u32  , depth_or_array_layers:u32    ){
    let parameters = TestParameters::default();
    initialize_test(parameters, |ctx| {
        let tex = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            dimension: wgpu::TextureDimension::D2,
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            format,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            mip_level_count: mips_count,
            sample_count: 1,
            view_formats: &[],
        });

            // let mip_w = w / (1 << mip_level);
            // let mip_h = h / (1 << mip_level);
            let val = (mip_level + 1) as u8;

            // let mip_size = mip_w* mip_h; 

            let data = vec![val; total_size_in_bytes as usize ];

            // Write the first two rows
            ctx.queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: &tex,
                    mip_level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                bytemuck::cast_slice(&data),
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(rows_per_image),
                },
                wgpu::Extent3d {
                    width:w  ,
                    height: 2,
                    depth_or_array_layers,
                },
            );

            ctx.queue.submit(None);

            let read_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: (total_size_in_bytes) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let mut encoder = ctx
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

            encoder.copy_texture_to_buffer(
                wgpu::ImageCopyTexture {
                    texture: &tex,
                    mip_level,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::ImageCopyBuffer {
                    buffer: &read_buffer,
                    layout: wgpu::ImageDataLayout {
                        offset: 0,
                        bytes_per_row: Some(bytes_per_row),
                        rows_per_image: Some(rows_per_image),
                    },
                },
                wgpu::Extent3d {
                    width: w,
                    height: h ,
                    depth_or_array_layers,
                },
            );

            ctx.queue.submit(Some(encoder.finish()));

            let slice = read_buffer.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| ());
            ctx.device.poll(wgpu::Maintain::Wait);
            let data: Vec<u8> = slice.get_mapped_range().to_vec();

            for byte in &data[..(total_size_in_bytes as usize )] {
                assert_eq!(*byte, val);
            }
            for byte in &data[(total_size_in_bytes as usize )..] {
                assert_eq!(*byte, 0);
            }
    });
}

#[test]
#[wasm_bindgen_test]
fn write_texture_subset_2d() {
    let size = 256;
    let parameters = TestParameters::default();
    initialize_test(parameters, |ctx| {
        let tex = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            dimension: wgpu::TextureDimension::D2,
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            format: wgpu::TextureFormat::R8Uint,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            mip_level_count: 1,
            sample_count: 1,
            view_formats: &[],
        });
        let data = vec![1u8; size as usize * 2];
        // Write the first two rows
        ctx.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&data),
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(size),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d {
                width: size,
                height: 2,
                depth_or_array_layers: 1,
            },
        );

        ctx.queue.submit(None);

        let read_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (size * size) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &read_buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(size),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
        );

        ctx.queue.submit(Some(encoder.finish()));

        let slice = read_buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| ());
        ctx.device.poll(wgpu::Maintain::Wait);
        let data: Vec<u8> = slice.get_mapped_range().to_vec();

        for byte in &data[..(size as usize * 2)] {
            assert_eq!(*byte, 1);
        }
        for byte in &data[(size as usize * 2)..] {
            assert_eq!(*byte, 0);
        }
    });
}

#[test]
#[wasm_bindgen_test]
fn write_texture_subset_2d_mips() {

let mips_count = 3 ; 
let w= 2048 ;
let h= 2048 ;
let  depth_or_array_layers= 1;
    for mip_level in 0 ..mips_count {
   let mip_w = w / (1 << mip_level);
             let mip_h = h / (1 << mip_level);
          let    bytes_per_row = mip_w; 
          let    rows_per_image = mip_h; 
let total_bytes_in_copy = total_bytes_in_copy(wgpu::TextureFormat::R8Uint,bytes_per_row,rows_per_image,depth_or_array_layers);

println!("------------------------{total_bytes_in_copy:?}");
            // let mip_size = mip_w* mip_h; 
            test(wgpu::TextureFormat::R8Uint ,mip_w , mip_h,bytes_per_row,rows_per_image,total_bytes_in_copy
            ,mip_level,mips_count,depth_or_array_layers  ) ; 
    }
  

        


}

#[test]
#[wasm_bindgen_test]
fn write_texture_subset_3d() {
    let size = 256;
    let depth = 4;
    let parameters = TestParameters::default();
    initialize_test(parameters, |ctx| {
        let tex = ctx.device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            dimension: wgpu::TextureDimension::D3,
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: depth,
            },
            format: wgpu::TextureFormat::R8Uint,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            mip_level_count: 1,
            sample_count: 1,
            view_formats: &[],
        });
        let data = vec![1u8; (size * size) as usize * 2];
        // Write the first two slices
        ctx.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytemuck::cast_slice(&data),
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(size),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 2,
            },
        );

        ctx.queue.submit(None);

        let read_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: (size * size * depth) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let mut encoder = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &read_buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(size),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: depth,
            },
        );

        ctx.queue.submit(Some(encoder.finish()));

        let slice = read_buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| ());
        ctx.device.poll(wgpu::Maintain::Wait);
        let data: Vec<u8> = slice.get_mapped_range().to_vec();

        for byte in &data[..((size * size) as usize * 2)] {
            assert_eq!(*byte, 1);
        }
        for byte in &data[((size * size) as usize * 2)..] {
            assert_eq!(*byte, 0);
        }
    });
}
