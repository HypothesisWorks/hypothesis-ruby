# frozen_string_literal: true

RSpec.describe 'printing examples' do
  it 'adds a statement to the exceptions string' do
    expect do
      hypothesis do
        n = given integers
        expect(n).to eq(0)
      end
    end.to raise_exception(/Given #1/)
  end

  it 'adds multiple statements to the exceptions string' do
    expect do
      hypothesis do
        n = given integers
        m = given integers
        expect(n).to eq(m)
      end
    end.to raise_exception(/Given #1.+Given #2/m)
  end

  it 'includes the name in the Given' do
    expect do
      hypothesis do
        n = given integers, name: 'fred'
        expect(n).to eq(1)
      end
    end.to raise_exception(/Given fred:/)
  end
end
